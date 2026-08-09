# Troubleshoot loading, audio, MIDI, and editors

Start with the error returned by the API, then narrow the problem to loading, processing,
audio I/O, MIDI, isolation, or the editor. First prove that the plugin loads, then that it
processes, then add the audio device or UI.

## Confirm the plugin path and architecture

Pass the outer `.vst3` path, not a library buried inside the bundle. Check a path without
starting audio:

```rust
use vst3_host::simple;

fn main() -> vst3_host::Result<()> {
    let path = std::path::Path::new("/path/to/plugin.vst3");
    println!("path exists: {}", path.exists());
    println!("looks like a VST3 module: {}", simple::is_valid_plugin(path));

    let info = simple::get_plugin_info(path)?;
    println!("{} {} by {}", info.name, info.version, info.vendor);
    Ok(())
}
```

`is_valid_plugin` is a lightweight path/module check, not proof that initialization will
succeed. `get_plugin_info` loads plugin code and can therefore fail or crash in-process.
For an untrusted plugin, use [`Vst3Host::probe_plugin`](isolate-plugin-crashes.md#validate-a-plugin-before-loading)
or [crash-resistant discovery](discover-plugins.md#crash-resistant-discovery).

Common causes of `PluginNotFound` or `PluginLoadFailed` include passing the internal
`.dylib`, `.so`, or `.dll`; using a plugin for a different CPU architecture; a missing
vendor runtime; or an invalid VST3 bundle layout. Preserve the complete error while
diagnosing—the loader includes platform-specific detail when available.

## Separate processing from the audio device

If loading works but playback does not, process one block without CPAL. This distinguishes
a plugin/format problem from an output-device problem:

```rust
use vst3_host::{simple, AudioBuffers};

fn main() -> vst3_host::Result<()> {
    let mut plugin = simple::load_plugin("/path/to/plugin.vst3")?;
    let channels = plugin.output_channel_count();
    plugin.start_processing()?;

    let mut buffers = AudioBuffers::new(0, channels, 512, 44_100.0);
    plugin.process_audio(&mut buffers)?;
    println!("processed {} output channels", buffers.outputs.len());
    Ok(())
}
```

An instrument stays silent until it receives a note; process several blocks after sending
one because plugins can have attack time or latency. An effect fed zero input normally
produces zero output. Use `simple::play_with_input` for live input or fill
`AudioBuffers::inputs` when processing manually.

## Diagnose silent playback

Check these in order:

1. Confirm the plugin is an instrument, or provide input if it is an effect.
2. Send MIDI after playback starts and keep the returned `AudioHandle` alive.
3. Read `audio.output_levels()` to distinguish silent plugin output from device routing.
4. Try 44.1 or 48 kHz and the plugin's actual output-channel count.
5. Check the operating system's default output device and application volume/routing.

Lock-free convenience methods report queue overflow with `false`; do not discard that
signal when producing bursts of events:

```rust
# use vst3_host::{simple, midi::{MidiChannel, MidiEvent}};
# fn main() -> vst3_host::Result<()> {
# let audio = simple::play(simple::load_plugin("/x.vst3")?)?;
let accepted = audio.send_midi(MidiEvent::NoteOn {
    channel: MidiChannel::Ch1,
    note: 60,
    velocity: 100,
});
if !accepted {
    eprintln!("audio command queue is full");
}
# Ok(())
# }
```

Use `audio.midi_panic()` when a dropped note-off or application error leaves notes sounding.

## Diagnose ineffective parameter changes

Parameter IDs are plugin-defined and need not be contiguous. Obtain IDs from
`get_parameters()` or `find_parameter()` rather than guessing `0`. Values are normalized to
`0.0..=1.0`; use `format_parameter` to see the plugin's displayed value.

While playing, `audio.set_parameter(id, value)` returns `false` when its queue is full. The
full-lock alternative, `audio.lock().set_parameter(...)`, returns a detailed `Result` but
can contend with the audio callback.

## Diagnose process-isolation failures

Isolation requires a `vst3-host-helper` from the same build as the library. Ship it next to
your executable, set `VST3_HOST_HELPER_PATH`, or configure `Vst3HostBuilder::helper_path`.
A stale helper can fail because the private IPC format has no compatibility negotiation.

- `PluginCrashed` means the helper exited or disconnected; call `recover()` off the audio
  thread if recovery is appropriate.
- `PluginTimeout` means a command exceeded the response timeout and the helper was killed.
  Increase `response_timeout` only for a plugin known to initialize slowly.
- `ProcessError` covers helper startup, lookup, and protocol failures; preserve its message
  in user-visible diagnostics.

See [Isolate plugin crashes](isolate-plugin-crashes.md) for deployment and recovery details.

## Diagnose editor failures

First check `plugin.has_editor()`. Create standalone windows and embedded editors on the UI
or main thread. On macOS, AppKit rejects window creation from a worker thread.

Standalone editors are exercised in CI on macOS, Windows, and Linux/X11. Embedded editors
are most thoroughly verified on macOS; Windows and Linux/X11 embedding remains experimental,
and Wayland handles are unsupported. An isolated editor can open only on macOS and cannot
be embedded in the host window.

See [Open or embed a plugin editor](open-plugin-editor.md) for lifecycle and resize handling.

## Preserve useful diagnostics

The public error enum is `#[non_exhaustive]`, so match specific recovery cases and retain a
fallback arm:

```rust
use vst3_host::Error;

fn report(error: &Error) {
    match error {
        Error::PluginNotFound(path) => eprintln!("plugin path not found: {path}"),
        Error::PluginCrashed => eprintln!("isolated helper crashed"),
        Error::PluginTimeout => eprintln!("isolated helper timed out"),
        other => eprintln!("VST3 operation failed: {other:#}"),
    }
}
```

When reporting an issue, include the operating system and CPU architecture, plugin name and
version, whether isolation was enabled, the complete error, and the smallest reproduction
that still fails.
