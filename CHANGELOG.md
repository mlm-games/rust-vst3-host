# Changelog

All notable changes to `vst3-host` are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project aims to follow
[Semantic Versioning](https://semver.org/) (pre-1.0: new features bump the minor version).

## [Unreleased]

### Fixed (second review pass)

Three more reviewers went at the crate — one debugging a real crash, one attacking the changes
above, one running adversarial inputs through the public API. Everything below was reproduced.

- **Four regressions from the fixes below, before they ever shipped in a release.** The chunked
  `process()` panicked on the audio thread when the caller's channel Vecs had unequal lengths (the
  block length comes from the first channel, so a shorter later channel got sliced past its end);
  it span forever if `block_size` was 0; gating the helper/probe ancestor search on
  `debug_assertions` broke every isolation test in release builds, because test binaries live in
  `target/<profile>/deps` where no other resolver looks; and the inspector's WAV-export toast was
  erased by the reload it triggers, so the export result was never visible at all.
- **`Vst3HostBuilder::build` validates sample rate and block size**, matching
  `Plugin::reconfigure`. The two disagreed: `block_size(0)` gave permanent silence and
  `sample_rate(0.0)` reached `setupProcessing`, where plugins computing `1.0 / sampleRate` produce
  NaN coefficients.
- **`RmsWindow` could be latched to silence permanently by one bad sample.** The running sum is an
  accumulator, so a NaN (or a sample large enough that its square overflows) poisoned it forever —
  evicting the entry subtracts NaN again — and `rms()`'s `max(0.0)` guard renders NaN as `0.0`. A
  meter would read digital silence for the rest of its life while full-scale audio flowed through.
  `RmsWindow::from_duration` also panicked with a capacity overflow on a non-finite duration.
- **A symlink loop made plugin scanning hang forever.** `scan_directory` recursed on `is_dir()`,
  which follows symlinks, with no visited set — so a plug-in folder containing a link to an
  ancestor wedged the scan while the result list grew unboundedly with textually distinct
  duplicates that `dedup` couldn't collapse. Directories are now tracked by canonical path.
- **Automation could stop audio rendering entirely.** Only the interpolating branch of
  `value_at_time` clamped, so a curve whose first or last point was out of range returned it raw to
  `Plugin::set_parameter_at`, which rejects it — and `Timeline::drive_block` propagates that
  *before* processing audio, so rendering stopped once the playhead reached the last point, losing
  that block's MIDI with it. All branches now clamp and map non-finite to a usable value.
- Overflow panics reachable from public APIs: `Timeline::advance_block` after a large
  `seek_frame`, `ParameterAutomation::points_for_block` with an absurd block length, and
  `midi::name_to_note` on an extreme octave (`"C2147483647"`).
- The inspector serviced the Linux editor run loop with a raw `try_lock`, which treats a poisoned
  mutex as a failed race — permanent, not transient — so a VSTGUI editor would stop painting for
  the rest of the session after any audio-thread panic.

### Changed

- `Vst3Host::discover_plugins` now documents that it instantiates every candidate **in-process**,
  so a plugin that aborts during its own init takes the host down with it — verified with a
  licensed Waves plugin that aborts with "Rust cannot catch foreign exceptions" during its license
  check. [`discover_plugins_safe`] exists for this and should be preferred.

### Fixed

Found by a five-lens review of the whole crate; each was confirmed by reading the code, and two
were reproduced standalone.

- **Dropping a `Plugin` with its editor open never detached the view.** Teardown deactivated and
  `terminate()`d the controller, then released the `IPlugView` as a plain field drop — so a view
  that was still `attached()` was destroyed without `removed()`, leaving the plugin's platform
  frame and idle timer pointing at a host window the caller was about to free. `open_editor` /
  `close_editor` paired correctly; the drop path didn't. It now detaches first.
- **A failed `process()` skipped every per-block cleanup.** The early return bypassed the input
  event clear, the input parameter queue clear *and* the pending-change clear (the comment claiming
  otherwise was wrong), so a plugin returning a failure code got every stale event re-delivered on
  each subsequent block — re-triggering held notes — while the queues grew without bound. Cleanup
  now always runs, and the error is a non-allocating `Error::ProcessFailed(tresult)` instead of a
  `format!` on the audio thread (`Error::NotProcessing` likewise).
- **`Plugin::note_off(NoteId)` sent pitch 0 on channel 1.** The event carried only the note id, so
  any synth that matches releases by pitch — most non-MPE synths — never released the real note.
  Note-ons are now tracked (bounded, pre-reserved, no audio-thread allocation) so the release
  reproduces the original channel and pitch.
- **A caller block larger than the configured block size left the tail silent.** The plugin may
  only be handed `maxSamplesPerBlock` at a time and the excess was simply dropped, which is
  reachable in practice: the cpal backend clamps the requested buffer size *up* into the device's
  supported range, and `BufferSize::Default` lets the device choose. That produced an audible gate
  at the device's block rate and a transport advancing at a fraction of real time. Oversized blocks
  are now split into successive chunks; queued events apply to the first chunk.
- **`midi::name_to_note` panicked on non-ASCII input.** It sliced at byte index 1 to detect a flat,
  which is not a char boundary once `to_uppercase` produces a multi-byte char — so `name_to_note("éB3")`
  panicked instead of returning `None`, from a safe `Option`-returning parser fed by text fields.
  Now parsed by chars.
- **Stepped-parameter helpers used the wrong `stepCount` convention.** VST3 counts the gaps between
  discrete values, so a toggle is `1` and an N-value list is `N-1` — the convention this crate
  already relies on in `select_program` and that `test-plugin` uses. But `is_boolean()` tested `== 2`
  and `is_discrete()` tested `> 1`, so a three-way selector was reported as a toggle (its third
  value unreachable through `format_value`) while a real bypass toggle was reported as continuous.
  `is_boolean()` is now `== 1`, `is_discrete()` is `>= 1`, `normalized_to_plain` snaps toggles too,
  and the new `Parameter::step_index` reports which of the `step_count + 1` values is selected.
- **The helper and probe lookup no longer executes binaries found above the executable in release
  builds.** Both resolvers walked up ancestor directories and ran any `target/{debug,release}/…`
  they found — a fine convenience in a cargo tree, an arbitrary-code-execution foothold in a shipped
  app, since the spawned binary is then trusted for everything the host believes about a plugin.
  The ancestor walk is now `debug_assertions`-only; release builds use the explicit path/env
  override or a binary beside the executable.

### Changed

- `Error` gained `ProcessFailed(i32)` and `NotProcessing`. The enum is `#[non_exhaustive]`, so
  matching code is unaffected, but text that used to arrive as `Error::Other` now has its own
  variants.

### vst3-inspector

- The plugin is now loaded on the UI thread. Only introspection runs on a worker — the library's own
  `docs/explanation/threading.md` requires `load_plugin` to run on the thread that will drive the
  GUI, warning that a plugin which builds its controller's resources elsewhere crashes when its
  editor is opened. The inspector was doing exactly that, and superseded loads were dropped (full
  COM teardown) on the worker thread too.
- WAV export no longer always claims the reload failed. It inferred success by checking `self.audio`
  immediately after starting a load that is asynchronous, so the check always saw `None`: users
  always got "reloading the live plugin failed" and never the real export result, and the state
  snapshot it meant to restore was silently discarded. The snapshot now rides along with the reload
  and is applied when it completes. Export also closes the editor window first, so the old instance
  is really gone before a second one loads.

## [0.8.0] - 2026-07-27

### Added

- **Linux `IRunLoop` host service, so VSTGUI-based plugin editors work.** The host frame
  (`HostPlugFrame`) now also implements `Steinberg::Linux::IRunLoop`: the plugin's editor
  registers its file-descriptor event handlers (its X11 connection) and periodic timers with
  the host, which are serviced by a new `Plugin::service_run_loop()` — call it on the UI
  thread every frame (~30–60 Hz) while an editor is open. Without this, VSTGUI-based editors
  (and most non-JUCE UIs) attach but never paint or respond; JUCE-based editors were
  unaffected because they drive their own event loop. Verified end-to-end against sfizz
  (VSTGUI): the editor paints, its knobs and file dialog respond, and audio renders. Timers
  and fd handlers are serviced snapshot-then-invoke so a callback can safely re-register from
  inside itself (VSTGUI does). No-op on non-Linux and under process isolation. Contributed by
  [@Rodvader8](https://github.com/Rodvader8) ([#10]).

### Fixed

- **`outputParameterChanges` is cleared before each process block.** The container is reused
  across blocks; without a reset, a plugin emitting output parameter changes for the same id
  every block kept being handed its own already-active queue and appending to it — mixing
  stale points into later blocks, making sample offsets describe more than the current block,
  and growing point storage without bound (so the audio thread could reallocate long after
  warm-up). The clear resets the active count only, so pooled queues keep their capacity and
  the reset stays allocation-free. Reported by [@Boscop](https://github.com/Boscop) ([#8],
  fixed in [#9]).
- The Linux run-loop registry is cleared in `close_editor`, so a plugin that doesn't
  unregister its own event handlers or timers can't leave the host dispatching into a removed
  view on the next `service_run_loop()`.
- **Host-side buffers a plugin fills are now all bounded.** A sweep for more of the
  unbounded-growth class [#8] reported found four more buffers with the same shape — written
  from the plugin side, drained only by an optional host poll. They now share the policy
  outgoing MIDI already used (`MAX_OUTPUT_MIDI`): pre-reserved to the cap so steady-state
  pushes never reallocate, and pushes past it dropped.
  - The `IComponentHandler` gesture log grew for the plugin's lifetime unless the host called
    `Plugin::take_parameter_edits()`, which is optional — dragging one knob appends per UI
    frame.
  - The raw `performEdit` sink grew the same way while the plugin wasn't processing.
  - The editor-parameter stash was appended **on the audio thread** every block and drained
    only by `Plugin::get_parameter_changes()`, which `RealtimePluginRunner` never calls — so
    the runner documented as allocation-free in steady state would grow and reallocate on the
    audio thread for as long as an editor stayed open.
  - `HostEventList` had no cap and `process()` (its only drain) returns early while stopped,
    so queueing MIDI at a stopped plugin grew without bound and then dumped every stale event
    into the first block once it started.
- Drains no longer `mem::take` their buffers, which left a zero-capacity `Vec` behind so the
  next push reallocated — the anti-pattern `HostEventList::for_each_then_clear` already
  documented avoiding.

### Known limitations

- Nothing drains editor feedback in the realtime path, so a `play_realtime` host sees no
  editor parameter changes (they are bounded and discarded rather than accumulating). Giving
  `RtAudioHandle` a parameter-feedback ring like `AudioHandle`'s is still to do.
- GUI across the process-isolation boundary is still not implemented, and
  `Plugin::service_run_loop()` is a no-op for isolated plugins.

## [0.7.0] - 2026-07-14

### Added

- **Full `PluginInternal` parity for process-isolated plugins.** Everything the in-process
  path supports now marshals across the IPC boundary:
  - `Plugin::reconfigure` and `Plugin::set_process_mode` (`kRealtime`/`kOffline`), so
    faster-than-real-time offline bouncing works out-of-process. Contributed by
    [@ro-ag](https://github.com/ro-ag) ([#7], fixes [#6]).
  - `bus_arrangements` / `set_bus_arrangements` (speaker-layout query and negotiation),
    `get_units` (unit/program-list enumeration — previously silently empty),
    `latency_samples` / `tail_samples` (previously silently `0`), and
    `midi_cc_to_parameter` (`IMidiMapping` — previously silently `None`).
- **Crash recovery replays the runtime config.** `Plugin::recover()` (and auto-recover)
  reloads a respawned helper at the last `reconfigure`d sample rate / block size and
  re-applies a non-default process mode, instead of silently reverting to the load-time
  settings. Verified end-to-end by a capstone that kills the helper and proves the rendered
  pitch is unchanged.
- MIDI input example (`examples/`).

### Fixed

- The isolation helper sized `process` output buffers as *bus count × 2* (a stereo-per-bus
  assumption), silently dropping channels beyond two per bus after a negotiated non-stereo
  arrangement; it now uses the plugin's live per-bus channel count.
- `Plugin::output_channel_count()` on an isolated plugin returned the load-time snapshot
  forever; a successful `set_bus_arrangements` now refreshes it from the arrangement the
  plugin actually applied.
- The helper committed its tracked sample rate before the plugin accepted a `Reconfigure`;
  a rejected reconfigure no longer desyncs the post-crash reload rate.
- `Plugin::reconfigure` rejects `block_size > i32::MAX` instead of silently truncating it in
  the internal casts.

## [0.6.1] - 2026-07-08

### Fixed

- **Memory leak: host-owned COM objects leaked a reference on every `process()` setup.** The
  host handed borrowed, host-owned COM objects to the plugin with `to_com_ptr().into_raw()`,
  which transfers a `+1` reference that is never released. The affected pointers — `ProcessData`
  input/output event lists and parameter changes, the `IComponentHandler` passed to
  `setComponentHandler`, and the `IParamValueQueue` pointers returned from `IParameterChanges` —
  are only borrowed for the duration of the call, so they are now passed as borrowed pointers via
  `as_com_ref().as_ptr()`. The `queryInterface` sites are unchanged: returning ownership there is
  required by the COM contract. LeakSanitizer reported 944 bytes leaked in 17 allocations before
  the fix; the sanitizer suite is clean after. ([#4])
- Bumped the `crossbeam-epoch` lockfile entry to 0.9.20 to clear an advisory flagged by
  `cargo-deny`.

## [0.6.0] - 2026-07-06

### Added

- **Timeline / sequencer engine** (`transport` module): schedule MIDI clips (`MidiClip`) and
  parameter-automation lanes (`AutomationLane`) on a beat grid and drive them into a plugin
  sample-accurately. `Timeline::advance_block` resolves the next block to `(event, sample_offset)`
  and `(param_id, sample_offset, value)` lists; `Timeline::drive_block` pushes them into a
  `Plugin` and renders one block. Beats timebase at a constant tempo (a tempo curve is future
  work). Exported from the crate root and prelude.
- `RtControl::send_midi_at`, `AudioHandle::send_midi_at`, `MidiSink::send_midi_at`, and
  `HostCommand::SendMidiAt` (isolation) carry a MIDI sample offset end-to-end, so scheduled notes
  land at the same block position on the in-process, lock-free, and isolated paths (the realtime
  and mutex playback bridges previously dropped the offset to block start).
- `Plugin::output_midi_handle()` returns an [`OutputMidiConsumer`] — a `Send` + `Sync` handle
  for draining the MIDI a plugin emits (arpeggiators, MPE, thru) from any thread without locking
  the audio thread. Pairs with `RealtimePluginRunner`: the lock-free runner previously had no way
  to surface output MIDI; now you take the handle, move the plugin into the runner, and poll from
  your UI thread while the audio thread pushes. In-process only (`None` under process isolation,
  where output MIDI crosses the boundary in IPC responses).
- **Android build support**: the crate now compiles for `target_os = "android"` (verified
  on-device on `arm64-v8a`, Android 13). Android reuses the Linux VST3 module path — `dlopen`
  plus the required `ModuleEntry`/`ModuleExit` entry points — so discover → load → parameters →
  `process_audio` all run natively on a phone through the library's public API. Plugin-editor
  windowing is not supported on Android (`PluginWindow::open` returns an error; there is no X11).

### Changed

- Output MIDI is now buffered in a lock-free bounded queue (`crossbeam-queue`) instead of an
  `Arc<Mutex<Vec>>`, so capturing emitted events takes no lock on the audio thread (oldest event
  dropped when full, as before).

- `RealtimePluginRunner::process` is now allocation-free and `Drop`-free in steady state — no
  per-block heap allocation, reallocation, or free once warmed up, even while parameter changes
  and MIDI (in and out) flow — given a fixed-size buffer and an in-process plugin. Fixed the
  three steady-state churns: `pending_param_changes` (drained in place instead of `mem::take`),
  the per-block `ParameterValueQueue` recreation (now a reuse pool), and output-MIDI capture
  (drained in place into a pre-reserved buffer). Guarded by `tests/alloc_tests.rs`. Not yet
  lock-free (a few short, uncontended mutexes remain per block); see the `RealtimePluginRunner`
  docs.

## [0.5.0] - 2026-06-23

### Added

- **Program selection** — `Plugin::select_program(unit_id, program_index)` selects a program
  from a unit's `IUnitInfo` program list (via the unit's `kIsProgramChange` parameter), and
  `MidiEvent::ProgramChange` is now honored instead of rejected (routed to the root unit's
  program-change parameter; the MIDI channel is ignored, as VST3 has no channel→unit mapping).
  Marshaled across process isolation.
- **Bus activation** — `Plugin::set_bus_active(media_type, direction, bus_index, active)` calls
  `IComponent::activateBus`, unlocking sidechain/aux/surround buses that must be explicitly
  activated. New public `audio::MediaType` and `audio::BusDirection` enums. Marshaled across
  process isolation.
- **Parameter-edit gesture capture** — `Plugin::take_parameter_edits()` drains an ordered log of
  the plugin GUI's `beginEdit`/`performEdit`/`endEdit` callbacks as `ParameterEdit` /
  `ParameterEditKind`, so hosts can record automation gestures, not just final values (which
  `get_parameter_changes` still returns). Marshaled across process isolation.
- **Runtime transport mutation** — `Plugin::set_tempo`/`set_time_signature`/`set_playing` change
  the `ProcessContext` transport on the next block while processing. Lock-free equivalents on
  `AudioHandle` and `RtControl` apply the change from the audio callback without a lock.
  Marshaled across process isolation.
- **Live MIDI input** (feature `midi-input`) — `midi_input::list_midi_input_ports`, `connect`,
  and `bind_to_handle` bind a hardware/virtual MIDI port (via `midir`) and forward parsed events
  into a running `AudioHandle`. New `AudioHandle::midi_sink()` returns a `Send` `MidiSink` for
  cross-thread MIDI injection. Off by default; additive when disabled.
- Export `AutomationCurve` and `AutomationPoint` from the crate root and prelude — they were
  public but unreachable without the full module path, even though `ParameterAutomation::with_curve`
  takes an `AutomationCurve`.

### Documentation

- Synced the user docs with 0.4.x: documented the crash-resistant discovery API
  (`discover_plugins_safe`, `SafeDiscoveryReport`, `probe_timeout`, the `vst3-host-probe`
  binary), corrected stale "MPE is in-process only" claims (it now works across process
  isolation), and fixed the "no GUI across the boundary" note (macOS isolated editors work).

## [0.4.2] - 2026-06-23

### Changed

- Inspector loads plugins on a background thread: introspection, load, and audio start run off
  the UI thread, so a slow or hanging plugin (e.g. Reason Rack) no longer freezes the window —
  it shows a "Loading…" spinner and stays responsive. (A crashing plugin still aborts the
  process; only process isolation contains crashes.)
- Documented and test-locked the process-isolation hang guarantee: a plugin that hangs the
  helper (including during load) yields a timeout error and the helper is killed, so it can't
  block the host. A plain in-process `load_plugin` is synchronous and cannot be bounded — a
  hanging plugin (e.g. Reason Rack, which expects its host ecosystem) blocks the caller, so
  load it with process isolation.

## [0.4.1] - 2026-06-23

### Fixed

- Restore the `IPluginBase::terminate()` teardown that 0.4.0 dropped. 0.4.0 skipped `terminate()`
  to work around a crash in Waves/WaveShell's own `terminate()`, but that regressed plugins
  which **require** it to break their controller↔component link before release — e.g. Jup-8000
  and Analog Lab V crashed on unload. `terminate()` is the spec-compliant teardown and is needed
  by most (especially dual-component) plugins, so it is called again. WaveShell loads and runs
  but can crash intermittently on unload (a Waves packaging bug); use process isolation if a
  clean unload matters for such plugins.

## [0.4.0] - 2026-06-23

### Added

- **Crash-resistant plugin discovery**: `Vst3Host::discover_plugins_safe()` (and
  `discovery::discover_plugins_safe` / `probe_plugin_info_isolated`) introspect each plugin in
  a throwaway `vst3-host-probe` subprocess, so a plugin that `abort()`s or makes a
  pure-virtual call during instantiation is skipped (reported in `SafeDiscoveryReport.skipped`)
  instead of taking down the host. `Vst3HostBuilder::probe_timeout` bounds each probe. The fast
  in-process `discover_plugins` is unchanged. (Trades scan speed for safety — one process per
  plugin.)
- **Per-note expression (MPE) across process isolation**: `note_on` / `note_off` /
  `send_note_expression` / `note_expressions` now marshal to the isolation helper, so MPE works
  the same in-process and out-of-process (previously the isolated path returned "not
  supported"). Verified end-to-end: a Tuning expression bends a voice an octave across the
  subprocess boundary.

### Removed

- `Vst3HostBuilder::auto_isolate_problematic` and the internal name-based "Objective-C
  conflict" detection. They auto-isolated plugins like Waves/WaveShell out-of-process by
  matching their filename — a band-aid for a crash that is now fixed properly (see below), so
  those plugins load in-process like any other. Explicit `with_process_isolation(true)` is
  unchanged for callers who still want isolation. The hardcoded Ozone discovery blacklist was
  also removed (Ozone loads fine).

### Fixed

- Plugins that crash in their own `terminate()` no longer take down the host. Some plugins —
  notably shell plugins like Waves' WaveShell — null-deref inside `terminate()` on teardown
  (an uncatchable native crash). `Plugin` teardown now stops processing, deactivates, and
  disconnects the component/controller, then drops the COM references (the plugin frees itself
  in its destructor via `Release`) — it no longer calls `terminate()`. Real hosts mitigate the
  same way. WaveShell, Ozone Imager, and Access Virus Editor now load and tear down cleanly
  in-process.
- Loading **or opening the editor of** an editor-style plugin no longer crashes the host.
  Three places created the plugin's editor view to probe it (the `has_gui` check at load,
  `has_editor()`, and `get_editor_size()` — the latter run by `PluginWindow::open()` before
  attaching) and then called `IPlugView::removed()` on the view. But those views were never
  `attached()`, which violates the `IPlugView` lifecycle. Plugins that only initialize their
  close state on attach (e.g. Access Virus Editor) segfaulted in `OnUIClose()`. The probe views
  are now released (dropped) without the spurious `removed()` call; only the real
  attach/detach path (`open_editor`/`close_editor`) calls `removed()`.

## [0.3.0] - 2026-06-22

### Added

- **Per-note expression (MPE)**: `Plugin::note_on` returns a `NoteId`, and
  `send_note_expression(id, NoteExpressionType, value)` targets per-voice expression
  (tuning / volume / pan / brightness / …) via VST3 note-expression events;
  `note_expressions()` discovers what a plugin supports (`INoteExpressionController`). Verified
  end-to-end against a new in-repo `test-plugin/` VST3 synth (`just test-plugin`) — a Tuning
  expression bends one voice an octave.
- `AudioHandle::try_lock` — non-blocking plugin lock for UI/render threads. Returns `None`
  when the audio callback holds the lock (held for each `process_audio` block) instead of
  stalling the caller.
- **Lock-free side channels on `AudioHandle`** so a UI/control thread never contends with the
  audio thread on the hot path: `send_midi` / `set_parameter` / `midi_panic` queue control onto
  a ring the callback drains before each block; `output_levels` (per-channel peak via atomics),
  `drain_output_midi`, and `drain_parameter_changes` read feedback the callback publishes after
  each block. The plugin mutex (`lock()`) is now only needed for rare state ops (preset
  save/load, WAV export, processing start/stop). `play`/`play_with_backend` wire this
  automatically.

### Changed

- Inspector: the Processing tab now scrolls instead of clipping content on a short window,
  secondary sections collapse, and the on-screen keyboard / VU meters are responsive to width.
- Inspector: defaults to the bundled `test_plugins/Dexed.vst3` at startup (was a hardcoded
  system plugin path), falling back to the last-loaded plugin if it isn't present.
- Inspector: repaints continuously so input is always processed promptly — a click that doesn't
  move the mouse no longer waits for the next event to register.
- Inspector: migrated off the egui 0.34 deprecated panel / rounding / `screen_rect` APIs.

### Fixed

- The builder's `sample_rate` / `block_size` are now applied to in-process plugins at load
  (they were ignored — plugins ran at the 44100/512 defaults while `Plugin::sample_rate()`
  reported the configured value).
- Inspector input lag / dropped clicks: the UI thread no longer touches the audio mutex during
  interaction at all. All control (MIDI, parameter edits) and all per-frame feedback (VU
  meters, MIDI monitor, editor parameter sync) now flow through the new lock-free
  `AudioHandle` side channels; the mutex is reserved for rare lifecycle ops. (An interim fix
  used `try_lock` for the per-frame reads.)
- Parameter edits made in a plugin's **own editor GUI** now affect the audio. The plugin
  reports these via `IComponentHandler::performEdit`; the host captured them for display but
  never routed them to the audio processor, so plugins that don't internally relay
  editor→processor changes (e.g. some dual-component synths) ignored GUI knob turns while
  presets still worked. `process()` now feeds those edits into the processor's input
  parameter queue.

## [0.2.1] - 2026-06-22

### Added

- `MidiEvent::from_midi_bytes` — parse a raw channel-voice MIDI message (status + data) into a
  `MidiEvent` (note on/off, CC, pitch bend, aftertouch), for forwarding hardware-controller MIDI.
- Inspector: a "MIDI Input Device" picker that forwards a connected controller's MIDI into the
  loaded plugin live — cross-platform via `midir` (CoreMIDI / ALSA / WinMM); device → plugin
  only, no feedback loop.

## [0.2.0] - 2026-06-22

A large, fully backward-compatible feature release: the public API only grows. Highlights are
real VST3 protocol coverage (transport, units, MIDI mapping, bus arrangements), live and
offline audio I/O, richer process isolation, metering, and a much more capable inspector.

### Added — library

- **Transport / `ProcessContext`**: the playhead now advances per block and advertises validity
  flags; `Vst3HostBuilder::tempo` / `time_signature` configure the transport.
- **Sample-accurate MIDI**: `Plugin::send_midi_event_at(event, sample_offset)`.
- **Sample-accurate parameter automation** is now carried across the process-isolation boundary.
- **Offline process mode**: `ProcessMode` + `Plugin::set_process_mode` (`kRealtime`/`kOffline`).
- **Runtime reconfigure**: `Plugin::reconfigure(sample_rate, block_size)`.
- **Bus-arrangement negotiation**: `audio::SpeakerArrangement` / `BusArrangements`,
  `Plugin::bus_arrangements` / `set_bus_arrangements`.
- **Units & programs**: `Plugin::get_units` (`IUnitInfo`).
- **MIDI controller mapping**: `Plugin::midi_cc_to_parameter` (`IMidiMapping`).
- **Latency / tail**: `Plugin::latency_samples` / `tail_samples`.
- **Presets**: `.vstpreset` load/save (`save_vstpreset` / `load_vstpreset`), JSON `PluginPreset`
  wrappers (`save_preset` / `load_preset`).
- **Live audio input / effect hosting**: `simple::play_with_input`, `play_with_input_backend`.
- **Offline render**: `simple::render_to_wav`, `render_to_wav_with_input`; dependency-free
  `audio::write_wav` / `read_wav`.
- **Test signals**: `audio::SignalSource` (sine / white-noise / WAV) + `InputSource`.
- **Metering**: `audio::PeakMeter` (falling ballistic + timed hold) and `RmsWindow`.
- **Denormal guard**: flush-to-zero / denormals-are-zero around processing (x86 MXCSR, ARM FPCR).
- **egui editor embedding** on Windows and Linux (in addition to macOS).
- **Process isolation**: configurable `Vst3HostBuilder::helper_path` (+ `VST3_HOST_HELPER_PATH`)
  and `response_timeout`; optional `auto_recover_plugins` with `Plugin::recovery_count`;
  bounded shutdown with SIGKILL fallback; GUI across the boundary on macOS.
- **Diagnostics**: actionable architecture-mismatch errors when loading a wrong-arch plugin on
  Windows/Linux (mirrors the macOS Mach-O diagnostic).
- **Input-stream buffer-size negotiation** for capture devices.

### Added — inspector

- Preset save/load, A/B preset compare, audio export to WAV, MIDI file (`.mid`) playback, and a
  parameter-automation demo.
- Session persistence (window size, last tab, MIDI channel, last-loaded plugin) and in-UI error
  surfacing.

### Changed

- Upgraded `egui` / `eframe` to 0.34.

### Fixed

- Playhead now advances during playback (was frozen).
- Crash when closing a plugin editor; UI input lag between events.
- Robustness hardening: poison-lock recovery, NaN-safe automation sort, empty-buffer RMS guard,
  non-finite meter input, MIDI offsets clamped to the actual block.
- Windows/Linux loaders resolve the `.vst3` bundle directory to the inner binary before loading.

### Docs

- New how-to guides: open/embed a plugin editor, sample-accurate automation, monitor audio
  levels; plus architecture and process-isolation explanations.

## [0.1.1] - 2026-06-21

- Relicensed to MIT and published to crates.io with no VST3 SDK build requirement (the `vst3`
  0.3 crate ships pre-generated bindings).

## [0.1.0] - 2026-06-21

- Initial release: safe VST3 hosting — discover, load, parameters, MIDI, audio playback, state
  save/restore, and process isolation.

[Unreleased]: https://github.com/HelgeSverre/rust-vst3-host/compare/v0.8.0...HEAD
[0.8.0]: https://github.com/HelgeSverre/rust-vst3-host/releases/tag/v0.8.0
[0.7.0]: https://github.com/HelgeSverre/rust-vst3-host/releases/tag/v0.7.0
[0.6.1]: https://github.com/HelgeSverre/rust-vst3-host/releases/tag/v0.6.1
[0.6.0]: https://github.com/HelgeSverre/rust-vst3-host/releases/tag/v0.6.0
[0.5.0]: https://github.com/HelgeSverre/rust-vst3-host/releases/tag/v0.5.0
[0.4.2]: https://github.com/HelgeSverre/rust-vst3-host/releases/tag/v0.4.2
[0.4.1]: https://github.com/HelgeSverre/rust-vst3-host/releases/tag/v0.4.1
[0.4.0]: https://github.com/HelgeSverre/rust-vst3-host/releases/tag/v0.4.0
[0.3.0]: https://github.com/HelgeSverre/rust-vst3-host/releases/tag/v0.3.0
[0.2.1]: https://github.com/HelgeSverre/rust-vst3-host/releases/tag/v0.2.1
[0.2.0]: https://github.com/HelgeSverre/rust-vst3-host/releases/tag/v0.2.0
[0.1.1]: https://github.com/HelgeSverre/rust-vst3-host/releases/tag/v0.1.1
[0.1.0]: https://github.com/HelgeSverre/rust-vst3-host/releases/tag/v0.1.0
[#4]: https://github.com/HelgeSverre/rust-vst3-host/issues/4
[#6]: https://github.com/HelgeSverre/rust-vst3-host/issues/6
[#7]: https://github.com/HelgeSverre/rust-vst3-host/pull/7
[#8]: https://github.com/HelgeSverre/rust-vst3-host/issues/8
[#9]: https://github.com/HelgeSverre/rust-vst3-host/pull/9
[#10]: https://github.com/HelgeSverre/rust-vst3-host/pull/10
