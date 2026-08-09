# Platform support

| Platform | Loading & audio | Plugin editor window | Notes |
| --- | --- | --- | --- |
| macOS | Working, exercised against real third-party plugins and the bundled TestSynth | Standalone window + embedded-in-egui (`PluginWindow`, `EmbeddedEditor`) | Primary development platform. |
| Windows | Builds + tested in CI with the bundled TestSynth | Standalone window exercised in CI; embedding implemented but not covered by the editor smoke test | Loading uses the Win32 module path. |
| Linux | Builds + tested in CI with the bundled TestSynth | X11 standalone window exercised under Xvfb; embedding implemented but not covered by the editor smoke test | Native editor support requires X11/XCB; Wayland handles are unsupported. |

CI builds and tests all three platforms. It builds the repository's TestSynth VST3 fixture
and opens its editor on macOS, Windows, and Linux (under Xvfb), including resize-handshake
regression tests. Third-party-plugin compatibility is still primarily exercised on macOS;
the CI fixture is not a guarantee that every vendor plugin works on every platform.

- **Editor embedding into egui** (`EmbeddedEditor`) is implemented for macOS, Windows, and
  Linux/X11. macOS is verified interactively; the Windows and Linux embedding paths remain
  experimental. Wayland window handles return an error.
- **Process isolation** (the helper binary) is exercised on macOS; the IPC is platform-neutral
  but hasn't been run on Windows/Linux.

> **Windows/Linux third-party plugins.** CI opens the bundled TestSynth editor through the
> Win32 and X11/XCB standalone-window paths. Broader real-world plugin coverage is still
> needed. Building on Linux requires the `libxcb` development headers.

## Default plugin scan directories

| Platform | Directories |
| --- | --- |
| macOS | `/Library/Audio/Plug-Ins/VST3`, `~/Library/Audio/Plug-Ins/VST3` |
| Windows | `C:\Program Files\Common Files\VST3`, `C:\Program Files (x86)\Common Files\VST3` |
| Linux | `/usr/lib/vst3`, `/usr/local/lib/vst3`, `~/.vst3` |

Add your own with `Vst3Host::builder().add_scan_path(...)`.

## Build requirements

- No VST3 SDK needed. The `vst3` dependency (0.3) ships pre-generated bindings, so a plain
  `cargo build` works — there is no `VST3_SDK_DIR` and no submodule to initialize.
- `libclang` is required at build time for `cpal`'s `coreaudio-sys` (macOS) and `alsa-sys`
  (Linux, which also needs the ALSA + libxcb dev headers).
- Audio output requires a working device; `play` opens the system default.
