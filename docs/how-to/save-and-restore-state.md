# Save and restore plugin state

Capture a plugin's full state (parameters, internal settings, loaded preset) to bytes and
restore it later — for presets, sessions, or undo.

## Save and restore

```rust
use vst3_host::simple;

# fn main() -> vst3_host::Result<()> {
let mut plugin = simple::load_plugin("/path/to/synth.vst3")?;

// Save: an opaque byte blob holding everything the plugin serialized.
let state: Vec<u8> = plugin.save_state()?;

// ... change parameters, load a different preset, etc ...

// Restore exactly what was saved.
plugin.load_state(&state)?;
# Ok(())
# }
```

**Treat the bytes as opaque** and pair them with the plugin's identity
([`PluginInfo::uid`](https://docs.rs/vst3-host)); they only mean something to the same plugin.
Persist them however you like (file, database, your session format).

## What's in the blob

VST3 splits a plugin into a processor and a controller, and each serializes its *own* state.
A save captures both: `IComponent::getState` (the DSP) and `IEditController::getState` (the
editor's own settings — scroll position, which page is open, meter ballistics). Restoring
only the component leaves the editor out of sync with the sound.

Because it carries two streams, `save_state` no longer returns the plugin's component bytes
verbatim. It returns a small versioned envelope — a `VST3HOST_STATE` magic, a version, and
the two payload lengths — followed by the component bytes and, when the plugin has a separate
controller, the controller bytes. Nothing about how you use the API changes; it is still an
opaque `Vec<u8>` in and out.

**Blobs saved by older versions still load.** `load_state` recognises the envelope by its
magic; anything without it is treated as a bare component stream and restored the way it
always was. Blobs saved by *this* version are not readable by older ones.

## Interchange with other hosts

`save_state` bytes are this library's own container. To exchange a preset with another VST3
host, use `save_vstpreset` / `load_vstpreset`, which write the standard Steinberg
`.vstpreset` container: the component state in a `"Comp"` chunk and the controller state in a
`"Cont"` chunk, tagged with the plugin's class id so a loader can refuse a foreign preset.

## Telling the plugin where the state came from

A plugin can ask *why* it is being restored. The host attaches an attribute list to the stream
it passes to `setState`, and the plugin reads `StateType` off it — the SDK's
`Vst::Helpers::isProjectState()` does exactly that. Plugins use the answer to restore
differently: a preset should not drag a session's per-instance settings along with it.

The library sets that for you:

| Call | `StateType` | `FilePathString` |
| --- | --- | --- |
| `load_state` | `Project` | — |
| `load_vstpreset(path)` | `TrackPreset` | the `.vstpreset` path |
| `load_preset(path)` | `TrackPreset` | the preset file's path |

`Project` is the SDK's "restored from a project loading". `TrackPreset` is the value that
makes `isProjectState()` answer "this came from a preset" — the remaining value, `Default`,
means specifically *the plugin's default state*, which a preset the user picked is not.

If your host holds preset bytes it loaded some other way, say so explicitly:

```rust
use vst3_host::plugin::StateContext;

# fn main() -> vst3_host::Result<()> {
# let mut plugin = vst3_host::simple::load_plugin("/path/to/synth.vst3")?;
# let bytes: Vec<u8> = plugin.save_state()?;
// From a file you read yourself — the plugin sees the path too.
plugin.load_state_with_context(&bytes, &StateContext::preset_from_path("/presets/lead.vstpreset"))?;

// From a preset you hold in memory (a database row, a network fetch).
plugin.load_state_with_context(&bytes, &StateContext::preset())?;
# Ok(())
# }
```

This works identically under process isolation — the context crosses the boundary, so an
isolated plugin's `setState` sees the same attributes an in-process one does.

## Notes

- **Call on the main thread.** State maps to the plugin's `getState`/`setState`; do it on the
  thread you load and drive the plugin from, not the audio thread.
- **Works under process isolation too.** The blob marshals across the boundary, so an
  isolated plugin saves/restores just like an in-process one.
- **Use it with crash recovery.** [`Plugin::recover()`](isolate-plugin-crashes.md) reloads an
  isolated plugin from its *default* state — snapshot with `save_state` first and `load_state`
  after to keep the user's settings.
- **Different plugins reject foreign bytes.** Passing state from plugin A to plugin B is
  undefined (the plugin decides what to do with bytes it doesn't recognize).

To export human-readable metadata (not the opaque state) for tooling, see `PluginReport` /
the inspector's "Copy JSON" instead.
