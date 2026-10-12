# TODO

Open items from the review of the `audio-blocks` branch (2026-10-12).
Fixed: items 1–4 of that review (level-measure deadlock, decode priority,
compound fade-out, stale on-disk levels), timestamp gaps and seeking in
resampled files.

## Audio: correctness

- **AAC at 44.1 kHz, last ~10 ms**: after a seek it differs from a decode
  from the start by up to 1e-3, resampled or not, already before the
  seek rework (48 kHz AAC is exact). Probably the end trimming of the last
  frames.
- **Timestamps that overlap** (jump backwards): both readers keep the
  samples written first, so a seek landing inside the overlap keeps the
  other copy. Gaps are exact.
- **Timestamps that drift** from the sample count (no clean gap): a decode
  from the start jumps by 5 ms each time the drift reaches it, a seek
  places by timestamp: the two can differ by up to 5 ms.
- **Cached levels in memory.** A file overwritten during the session keeps
  its level until restart (the disk cache checks the files, the in-memory
  one does not). Same as the other derived caches, keyed by the
  import-time `content_hash`.
- **Pitch-corrected compound containing itself.** The stretch path drops
  `Readiness::Cyclic`: up to `MAX_COMPOUND_DEPTH` layered copies and one
  stretch job per depth instead of silence. Only if a cycle can be built.
- **`MixSnapshot::is_ready`** is true while a pitch-corrected clip's stretch
  is pending (the clip is skipped, not counted).
- **Failures cached forever.** A decode error marks the stream `missing`
  until it leaves the timeline; `reads_from_start` caches its probe; a
  failed level measure is `Ready(0.0)`, i.e. unity gain, with only an
  `eprintln`. A `recv` error in the export reader reads as end of file.
- **`SampleBlocks::to_vec`** loops forever on a block whose length is not a
  multiple of `channels` (no producer does that today; add `.max(1)`).
- **Weak seek anchor**: when none of the first `MAX_ORIGIN_PACKETS` packets
  gives two frames, the fallback anchor ignores priming and pre-skip (off by
  up to a frame). Log it at least.

## Audio: performance

- **Stale stretch jobs** run to completion: dragging the trim of a
  pitch-corrected clip queues a full render-and-stretch per drag frame.
  `send_jobs` clones the whole `Project` on every sync that queues a job.
- **Fast forward** starts a new mix-and-stretch thread per frame while
  editing, and every frame after a failed stretch (`timeline_audio.rs`,
  `request_window`).
- **Shutdown** runs every queued decode, level and stretch job to the end:
  nothing cancels them when `MixBufferCache` drops.
- **Previewing a media** from the pool drops every decoded block of the
  timeline (`begin(true)` clears `asked`), despite the `begin_media_pass`
  doc.
- **Export reader with several streams** buffers the other streams to EOF
  when one is much shorter than its clip window (`Reader::read`).
- **On-disk level cache** (`$XDG_CACHE_HOME/venturi/levels`) is never
  pruned; the preview's in-memory parts grow for the session.

## Audio: budget

- `rebalance` counts wanted blocks, not bytes held: a job kept for one
  block inserts all 8. Stretched clips are not counted.
- `audio_budget_mb` is not clamped when read from the settings file: 0
  plays the timeline silent.

## Audio: cleanup

- Two block decoders (`mix_buffers::decode_blocks`, export
  `Reader`/`KeptStream`).
- Cancel-and-forget code copied in `sweep_unused` and `rebalance`.
- `SampleBlocks::hash_identity` is unused, and its pointer hash can collide
  once a block is freed: remove it.
- Clip readiness is computed then dropped by every public constructor; the
  `normalize_gain` comment still mentions the gone compound cache.
- Misplaced doc comments: `eq.rs:62` (`EqResponse` lost its doc),
  `mix_buffers.rs` above `stretched` ("Whole mixdowns of…").
- The seek's first read repeats the packet-to-slot lookup of
  `decode_streams`.
- FFmpeg floods stderr ("Could not update timestamps for skipped samples")
  on every MP3/Opus seek: no `av_log_set_level` anywhere.

## Audio: tests

- Seek tests have no multi-stream or VBR MP3 case, and share a fixed
  fixture dir between concurrent runs.
- `export::mix_audio_track` uses `LevelCache::on_disk()`: tests write to the
  real cache dir, with fixtures sharing `content_hash: 1`.
- `stretch_bench_window` is timing-based and flaky under load.

## Audio: behaviour to keep in mind

- Nested effect state is matched by (track, clip id): moving a compound
  clip to another track restarts its compressor/EQ (possible click).
- After a seek into a compound clip its nested compressor starts from
  silence (master ran it from the compound's start).
- Split level-measure runs restart effect state with a 2 s preroll: the
  normalization gain can vary by ~0.1 dB with cache state and thread count.
- A cached segment's true peak at its edge was computed against the
  neighbour of that time, which is not in its key.
