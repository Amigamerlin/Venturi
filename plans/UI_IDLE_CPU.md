# UI_IDLE_CPU — the editor's main thread busy while nothing happens

**Status:** to investigate, nothing changed yet.

## 1. What was seen

2026-10-12, release build, Asahi (M1 Pro), the reference project
(`venturi-YT.vvproj`: master "Timeline 1", 29 min at 1080p60, 8 compound
clips, 58 OBS files) opened on its master timeline, playback stopped, no
input, window drawn on screen:

- the main thread uses **35–41% of a core, continuously**;
- every other thread is idle (≤ 2%);
- the same with the build before the audio work (`ddbcad8`: 35%) and
  after it (`3cb46fb`: 41%), so it is not the audio; the 6 points between
  the two are within the noise of one 10 s sample each, but are worth a
  second look (§3, step 5);
- measured from `/proc/<pid>/task/*/stat` over 10 s, with no MCP call
  during the sample (an MCP `get_state` every 0.5 s wakes the UI and was
  ruled out).

A cost paid on every frame with nothing to draw anew: battery, heat, and
less CPU for decoding during playback.

## 2. Hypotheses, cheapest to check first

1. **Continuous repaint.** Something calls `ctx.request_repaint()` every
   frame while idle (41 call sites in `vv-app`): audio meters, the
   "buffered" strip, render-ahead progress, the MCP server's wake-up,
   a status badge, an animation that never ends. egui then redraws at the
   display's rate.
2. **Per-frame work proportional to the project.** Run on every frame
   even when the frame is the same: `buffered_timeline_ranges` recursing
   into every compound clip (`cached_ranges_of_timeline`), waveform
   lookups for every clip, `active_video_clip_at`, media pool offline
   checks, layout of hundreds of clips, `TimelineAudio::sync`'s early
   return path, `sync_render_ahead`.
3. **Both**: an idle repaint is cheap on a small project and costs 40% of
   a core on this one.

## 3. Plan

1. **Frame rate while idle.** Count frames per second with no input (a
   temporary counter in `update`, or egui's `ctx.cumulative_pass_nr()`
   sampled over MCP if exposed). ~0 fps means hypothesis 1 is out.
2. **Who asks for the repaint.** If it repaints continuously: log the call
   site (wrap `request_repaint` / `request_repaint_after` in a debug helper
   with `#[track_caller]`, or bisect by disabling them), on the reference
   project and on an empty one.
3. **What a frame costs.** Time the main sections of `update` (timeline
   drawing, viewer, media pool, inspector, syncs) with `Instant` around
   each, summed over a few seconds; or `perf record -g` / `samply` if
   installed (neither was on 2026-10-12). Compare the reference project
   with a small one to find what scales with clips or compounds.
4. **Fix** the source: repaint only on change (the meters only while
   playing, the buffered strip when the cache changes, timers through
   `request_repaint_after`), cache per-frame results by
   `History::generation` and cache revision.
5. **Measure again** the same way as §1, before and after, on the
   reference project and an empty one; check that playback, scrub, the
   meters and the buffered strip still update.

## 4. How to measure (as in AUDIO_GRAPH §9)

- Work on a **copy** of the project, never the user's file. Opening it adds
  it to `recent_projects` in `~/.config/venturi/settings.json`: remove the
  entry afterwards.
- `target/release/vv-app --mcp copy.vvproj`; the socket is
  `$XDG_RUNTIME_DIR/venturi/mcp-<pid>.sock` (newline-delimited JSON-RPC,
  `initialize`, then `tools/call`). `set_active_timeline` opens the master.
- The window must be drawn (on another workspace the UI does not run):
  `screenshot_ui` errors otherwise; call it at the start and the end.
- Per-thread CPU: two snapshots of `utime + stime` (fields 14 and 15) of
  `/proc/<pid>/task/*/stat`, 10 s apart, **without** MCP calls between
  them.
