# AUDIO_GRAPH — the preview and export audio as a graph of nodes

**Status:** design, nothing implemented; what ships today and its limits
are in §1.3. Supersedes the audio part of
[REVERSE_SPEED.md](REVERSE_SPEED.md) (§3) and the open points of
[AUDIO_SEEK.md](AUDIO_SEEK.md).

**Goal:** one model in which every audio feature (sources, clips, speed,
reverse, effects, tracks, compound clips, analyses) is a node that declares
what it needs, so a new feature is one new node and never a change to the
cache, the scheduler or the memory budget.

---

## 1. Why

### 1.1 What happened

A user report: editing a 1080p60 project (`venturi-YT.vvproj`: a 29-minute
master timeline made of 8 compound clips, one per chapter; 58 OBS files,
4.7 GB, PCM 48 kHz with 3 audio streams each) pushed a 16 GB machine to
90% memory. Video cache budget, render-ahead window and 8-bit processing
made no difference: the memory was all audio. The work of 2026-10-11/12, in
order:

| Commit | Change | Master timeline open (RssAnon) |
|---|---|---|
| before | whole files decoded to f32 and never freed, plus FFmpeg packet leak | 7.3–9.5 GB, never released |
| f8952b5 | `Packet::read` leak fixed (audio decode kept every packet of the file, video decode leaked during playback) | — |
| 13ecb29 | decode only the ranges the clips read, free what a mix pass no longer reads | 2.0–2.2 GB |
| 34a3d07 | `SampleBlocks`: audio held in 64k-frame blocks, published per block | 1.1–1.3 GB |
| bfa36ba | sample-exact seeking decode, jobs closest to the playhead first, 1 GB budget | 1.33 GB |
| a5d3c48 | compound clips mixed by block (2 s preroll) instead of whole | 1.31 GB; 0.88 GB with a 256 MB budget |

Chapters went from 1.7 GB to 0.7–0.8 GB (of which ~0.55 GB is not audio).

### 1.2 Why a redesign and not a fifth patch

Every step above special-cased something: files, then compounds, then
"compounds that normalize" (still mixed whole: a block would measure only
itself, so a compound whose sources exceed the budget is **silent**). The
next features would add more cases:

- **Reverse** (REVERSE_SPEED): a clip whose block needs the *mirrored*
  source range.
- **Pitch-corrected speed**: already a separate whole-range path
  (`AudioSource::stretched`).
- Normalization, and any future analysis (noise profile, loudness match,
  auto-ducking), needs to *read* the whole input once, not to *hold* it.
- Look-ahead effects (limiter) need a little of the future, stateful ones
  (EQ, compressor) a little of the past.

The common shape: **for an output range, each element needs some input
ranges, or all of its input in order.** Making that the only contract
removes the special cases.

### 1.3 Current state (usable without this plan)

After the commits above plus the budget setting:

- **Memory**: bounded by the audio cache budget, **Settings > Playback >
  Audio cache** (default 1 GB, `Settings::audio_budget_bytes`). When the
  timeline's audio does not fit, the toolbar shows "Audio cache full" with
  a tooltip explaining the consequences and pointing to the setting.
- **Resampled sources** (anything not at 48 kHz, e.g. 44.1 kHz music or
  phone recordings) cannot seek without a phase offset, so every queued job
  of such a file is decoded in one read from its start
  (`mix_buffers::take_batch`), not one read from the start per job.
- **Verified** by tests (sample-exact seek on WAV, FLAC, PCM+H.264 MKV,
  AAC, Opus, MP3; block mix equal to the previous mix, compounds included)
  and by runtime measurement (§9). **Not verified by listening**: scrub and
  jumps (a short silence until the blocks near the playhead arrive; new
  blocks reach the mix with up to 250 ms of delay) and block edges inside
  compounds with a compressor or EQ (2 s preroll, an approximation).

Known limits, all removed by step 4 of §8:

1. A compound clip whose own timeline normalizes is mixed whole and needs
   all its sources at once: over budget it plays **silent**.
2. A normalization on a track or master measures only once all its audio
   is decoded: over budget it is never measured and the preview plays it
   at unity gain (or the last level measured). Export is unaffected.

---

## 2. Where the code is today

| Piece | Where | Role |
|---|---|---|
| `SampleBlocks`, `BLOCK_FRAMES` | `vv-audio/src/blocks.rs` | interleaved audio in equal blocks, holes play as silence, end known apart |
| `decode_audio_streams_from` | `vv-media/src/audio.rs` | seek + preroll, frames placed as a decode from the start would (anchor on the first frames, grid snap for ms timestamps, trimmed-first-frame fix); resampled streams decode from the start |
| `collect_clips`, `MixSnapshot`, `mix_range` | `vv-audio/src/mixer.rs` | walks a timeline, builds the immutable snapshot (clips + per-track `Insert` chains + master), mixes any range; `MixState` carries EQ/compressor state between calls |
| `AudioSource` | same | what the mixer asks: `file_frames(path, stream, frames, plays_at)`, compound blocks, stretch, levels |
| `compound_audio`, `compound_blocks`, `CompoundMix` | same | compound clip: by block with preroll, or whole when its timeline normalizes |
| `resolve_chain`, `LevelAnalysis` | same | effects of a strip; normalization measures through the effects before it |
| `MixBufferCache` | `vv-app/src/mix_buffers.rs` | preview store: file blocks, compound blocks, budget, `rebalance` by distance to the playhead, decode/mix/stretch/level workers, passes (`begin_pass`/`sweep_unused`); `take_batch` reads a resampled file once for all its queued jobs |
| `TimelineAudio` | `vv-app/src/timeline_audio.rs` | rebuilds the snapshot on history change or new blocks, publishes it to the cpal callback; fast forward (`StretchedWindow`), scrub, media preview |
| `mix_audio_track` | `vv-session/src/export.rs` | export: collects the wanted windows, decodes them from the start of each file, mixes; compounds whole |
| `analysis.rs`, `silence.rs` | `vv-session` | MCP levels and silence detection, on top of export's mix or direct decodes |

Facts to keep: preview and export share `mix_range`; gain keyframes are
evaluated per 800-frame block anchored on the clip start so a mix does not
depend on how it is split into calls; mute/solo come from
`Timeline::audible_tracks`; a level still measuring keeps the last one.

---

## 3. The model

### 3.1 Node

Everything that produces audio is a node; its output is audio in its own
time axis, at the project rate (48 kHz) and channel count.

```rust
/// Sketch, names to be settled in step 1.
trait Node: Send + Sync {
    /// Identity of the output: kind, parameters, and the hashes of the
    /// inputs. Equal hash, equal audio.
    fn hash(&self) -> NodeHash;
    /// Frames this node produces (`None`: unbounded, e.g. a timeline).
    fn len(&self) -> Option<u64>;
    /// What producing `out` needs from each input.
    fn demand(&self, out: Range<u64>) -> Vec<Demand>;
    /// Produces `out` from the input audio `demand` asked for.
    fn render(&self, out: Range<u64>, inputs: &Inputs, dst: &mut [f32]);
}

enum Demand {
    /// These frames of input `i`, any order, any time.
    Range { input: usize, frames: Range<u64> },
    /// All of input `i`, in order, once: an analysis or a
    /// whole-signal transform. Its result is cached by hash.
    Whole { input: usize },
    /// A value computed by an analysis node (a level, a profile).
    Value { input: usize },
}
```

Two dependency shapes only, both generic:

- **Range**: any function from output range to input ranges. Identity,
  shift, scale (speed), mirror (reverse), widen (pre-roll for state,
  post-roll for look-ahead). Evaluated per block, in any order, so the
  scheduler can start at the playhead and cache any block on its own.
- **Whole**: the input read once from start to end, streamed block by
  block with constant memory. Used by analyses (output: a small value) and
  by transforms that are not local (output: audio, cached in blocks like
  any other).

An analysis is its own node producing a **value** (`Demand::Value`), so the
node that applies it (a gain) stays a plain Range node. Normalization =
`Analyze(level) -> Gain(value)`; a future noise reduction =
`Analyze(profile) -> Denoise(value)`.

### 3.2 Stateful nodes and exactness

EQ and compressor keep state from one sample to the next. A Range node with
state declares a **preroll**: to render `[a, b)` it renders
`[a - preroll, b)` from a fresh state and keeps the tail (what
`CompoundMix::run` does today with 2 s). This is an approximation: equal to
a sequential render only once the state has converged. The preroll is per
node (a compressor's release, a filter's decay), not a global constant.

The **live** evaluation in the audio callback stays sequential, with
`MixState` carried between callbacks, so playback through stateful nodes
is exact. Blocks rendered with preroll appear only where a node is
materialized (§3.4). Tests compare them to a sequential render within a
stated tolerance; pointwise and mapping nodes must match exactly.

### 3.3 Hash: cache by content, not by memory address

`NodeHash` = hash of the node kind, its parameters and its inputs' hashes.
A file source hashes `MediaItem::content_hash`, the stream index, rate and
channels.

**Clip nodes work in clip-local time.** The clip node's output is the
clip's own audio from its first frame; placing it on the timeline is the
job of the track node. So moving a clip, rippling the timeline or editing
another clip does not change the clip's hash and keeps its cached blocks.
Trimming the end only changes `len`. Today a ripple delete in a compound
changes the compound's `content_hash` and drops every one of its blocks;
with node hashes only the tracks and mixes downstream of the edit change.

This also replaces the `LevelAnalysis::key` built from buffer addresses,
which made levels re-measure whenever blocks were evicted and decoded
again.

### 3.4 Materialized nodes

Not every node's output is cached. A node is **materialized** (its blocks
go in the store) when recomputing it is expensive or impossible from what
is at hand:

- file sources (decode);
- `Whole` outputs: stretched clips, reverse-of-a-long-GOP if it ever needs
  it, analysis values;
- compound clips (their whole nested graph);
- optionally any node the scheduler finds hot (later).

Everything between materialized nodes (clip gain and fades, track sum,
strip effects, master) is evaluated live, as `mix_range` does today.

### 3.5 Time mapping for priority

Each demand carries how the requested frames map to the **outermost**
timeline (`plays_at` today, with `placement` threaded through compounds).
A Range node maps it through its own mapping (a reverse node maps
backwards), so every block in the store knows when it plays, and the
budget keeps the blocks closest to the playhead, whatever chain they came
through.

---

## 4. Existing and planned features as nodes

| Feature | Node | Demand |
|---|---|---|
| Media file stream | `FileSource(path, stream)` | — (decoded, materialized) |
| Clip (source window, constant speed, varispeed) | `Clip` | Range: `[off + a·s, off + b·s + lookahead)` on the source |
| Clip fades, keyframed gain | part of `Clip` (pointwise) | — |
| Pitch-corrected speed | `Stretch(tempo)` | Whole on the clip's source window |
| Reverse (REVERSE_SPEED) | `Reverse(axis)` | Range: `[axis - b, axis - a)`, rendered backwards |
| Freeze frame | `Silence` (as today: frozen clips are not mixed) | — |
| Track: clips placed and summed | `Track` | Range per clip, shifted by `timeline_start` |
| Mute / solo | graph construction (`audible_tracks`) | — |
| Strip gain, pan, mono | `Gain`, `Pan`, `Mono` | Range, identity |
| EQ, compressor | `Eq`, `Compressor` | Range with preroll |
| Normalize, relative | `Analyze(mode)` on the strip input + `Gain(value)` | Whole + Value |
| Normalize, independent | one `Analyze` per clip + `ClipGains(values)` | Whole per clip + Value |
| Master | same as a track strip on the sum | — |
| Compound clip | `Timeline` sub-graph, materialized | Range (nested time) |
| Look-ahead limiter (future) | `Limiter` | Range with post-roll |
| Noise reduction with learned profile (future) | `Analyze(profile)` + `Denoise(value)` | Whole + Value |

Consumers outside the graph stay outside: fast forward stretches the live
output (`StretchedWindow`), scrub plays 80 ms of it, meters and spectrum
taps hang on track and master nodes in the live evaluation.

---

## 5. Store and scheduler

Generalize what `MixBufferCache` already does, keyed by node instead of by
file or compound:

- **Store**: `(NodeHash, block index) -> Arc<Vec<f32>>` plus
  `NodeHash -> value` for analyses. One byte budget for both. `rebalance`
  (distance of each wanted block from the playhead, keep what fits, drop
  the rest, plan what is missing) works unchanged with `Owner = NodeHash`.
- **Pass**: on every history change or batch of new blocks, the UI thread
  rebuilds the graph and walks demand from the live window (the whole
  timeline, as today) down to materialized nodes: that is the wanted set.
- **Jobs**: decode (FileSource, seeking), render (a materialized Range node
  block, once its inputs are there), stream (a Whole node, start to end,
  constant memory; it asks for its input blocks in order and may run ahead
  of the budget's choice since it frees them as it goes). Picked by
  distance to the playhead, cancelled when no longer wanted.
- **Workers**: today one decode, one mix, one stretch, one level worker.
  The job kinds above can share a small pool; size decided by measurement.
- **Invariants to keep** (tests exist for each in
  `vv-app/src/tests/mix_buffers.rs`): only the blocks read are decoded,
  with the same samples as a whole decode; nearby reads share blocks; a
  pass frees what it no longer reads; over budget the blocks closest to the
  playhead are kept; the job closest to the playhead runs first; a read
  past the end of a file ends at it.

---

## 6. Threads and real time

- UI thread: builds the graph on change (cheap: it holds `Arc`s), runs the
  pass, publishes `Arc<Graph>` to the callback (as `MixSnapshot` today).
- Audio callback: evaluates the live part sequentially, reading
  materialized blocks from the published graph. Never waits: a missing
  block is silence, as today. Old graphs are dropped on the UI thread.
- Workers: decode/render/stream jobs; results reach the store through
  `poll` on the UI thread.

---

## 7. Export and offline consumers

Export evaluates the same graph sequentially over its range: Whole nodes
stream first, then the timeline is rendered start to end with exact state.
`mix_audio_track` with its whole-compound mixdowns, `WantedFrames` and
`DecodedAudio` go away; export can decode by seeking as well, which closes
AUDIO_SEEK. `analysis.rs` (MCP levels) becomes an `Analyze` node on the
timeline output. `silence.rs` reads one stream through VAD and stays
outside the graph.

Preview and export are the same code by construction, not by convention.

---

## 8. Plan

Each step is shippable on its own and verified the way the 2026-10 work
was: unit tests, plus sample comparison against the code it replaces, plus
the runtime measurement of §9.

1. **Interfaces** (`vv-audio`): `Node`, `Demand`, `NodeHash`, the graph
   type, block evaluation helpers. No behaviour change. Decide the open
   points of §10 that block the code.
2. **Graph builder and live evaluation**: build the graph from a timeline
   (clips, tracks, strips, master, compounds as sub-graphs) and evaluate it
   sequentially. Acceptance: for every existing mixer test project and a
   few real projects, the output is **sample-identical** to `mix_range` on
   `collect_clips`'s snapshot. Keep both paths side by side behind a
   switch until step 3 lands.
3. **Store and scheduler on node hashes**: replace `MixBufferCache`'s
   file/compound special cases. Materialize FileSource and compound nodes.
   Acceptance: the `mix_buffers` invariants of §5 pass on the new store;
   compound blocks equal the sequential render within the stateful-node
   tolerance; runtime memory on the reference project no worse than
   a5d3c48 (master ≤ 1.35 GB at a 1 GB budget, ≤ 0.9 GB at 256 MB).
4. **Analysis nodes**: `Analyze` with streaming peak, true peak and
   loudness (BS.1770 gating fits a histogram), values keyed by node hash.
   Normalize moves to `Analyze + Gain`. The "compound that normalizes is
   mixed whole" path and `LevelAnalysis` go away. Acceptance: a compound
   that normalizes plays under a budget smaller than its sources (today
   silent); levels equal today's within float tolerance; a ripple delete
   elsewhere does not re-measure.
5. **Stretch as a Whole node**: replaces `AudioSource::stretched` and its
   worker.
6. **Export on the graph**: `mix_audio_track` evaluates the graph;
   seeking decode for export windows. Acceptance: exports bit-identical to
   the current ones for projects without stateful effects at block
   boundaries; within tolerance otherwise; export memory measured on the
   reference project.
7. **Reverse**: the `Reverse` node, the audio half of REVERSE_SPEED.
   Acceptance: a reversed clip equals the reversed samples of the forward
   clip, in preview blocks and in export.

Later, by measurement: f16/i16 block storage, a shared worker pool,
materializing hot intermediate nodes.

---

## 9. Measuring at runtime

The numbers above come from driving a release build over MCP on a **copy**
of the project (never the user's file):

1. `vv-app --mcp copy.vvproj`; connect to
   `$XDG_RUNTIME_DIR/venturi/mcp-<pid>.sock` (newline-delimited JSON-RPC,
   `initialize` then `tools/call`).
2. `set_active_timeline`, `delete_ranges` with `ripple: true`, and a
   `get_state` every 0.5 s to drive UI frames.
3. Sample `RssAnon` from `/proc/<pid>/status`, CPU per thread from
   `/proc/<pid>/task/*/stat`.
4. **The window must be drawn.** On another workspace MCP still answers but
   the audio sync does not run and every number is meaningless. Check with
   `screenshot_ui` at the start and the end of a run (it errors when the
   window is not drawn).

A temporary `eprintln!` in `sweep_unused` with the store's contents (bytes
per kind, wanted blocks, over budget) separated cache contents from
allocator effects twice in the 2026-10 work; remove it before committing.

---

## 10. Open decisions

- **Trait objects or a closed enum** for nodes. A trait keeps new effects
  local; an enum keeps the audio-callback path free of dynamic dispatch
  per sample. A trait with block-sized `render` calls makes the dispatch
  cost negligible.
- **Value inputs in the live path**: an `Analyze` value not ready yet
  plays the last known value for the same slot (today's behaviour), or
  unity on first use.
- **Preroll per node**: fixed per kind, or derived from parameters
  (compressor release, lowest EQ band).
- **Where clip-local time starts** for a clip whose source range is
  trimmed at the head: hashing the source window start rather than the
  clip id keeps cache hits across trims.
- **Budget accounting for Whole jobs**: a streaming job reads blocks far
  from the playhead; it must not evict the blocks near it.
- **Compound hashing**: a compound's node hash is its graph's hash, so two
  uses of the same compound share blocks; check against nested compounds
  and `MAX_COMPOUND_DEPTH` cycles.

## 11. Not in scope

Video (`render_ahead`) keeps its own cache; the same model (region of
interest per node) would fit it, but that is another plan. The mixer's
numeric behaviour (gain blocks, fades, varispeed interpolation) does not
change.
