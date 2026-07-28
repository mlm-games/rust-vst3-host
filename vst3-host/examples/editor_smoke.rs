//! Smoke check for the native plugin-editor path: load a plugin, open its editor in a real OS
//! window, service it for a few frames, close it. Prints what it did and exits `0` on success,
//! `1` on failure — so it can be wired into CI or a `just` recipe as a pass/fail gate.
//!
//! ```bash
//! cargo run -p vst3-host --example editor_smoke                       # test_plugins/Dexed.vst3
//! cargo run -p vst3-host --example editor_smoke -- /path/to/Some.vst3
//! RUST_LOG=vst3_host=debug cargo run -p vst3-host --example editor_smoke  # incl. resize traffic
//! ```
//!
//! Against the in-repo `TestSynth.vst3` it does more than survive: that plugin's view publishes
//! what it saw during the handshake as read-only parameters, so this run *asserts* the host
//! attached the view, offered it a content scale, and answered its `IPlugFrame::resizeView`
//! request with an `onSize` of exactly that size — the half of the editor protocol no
//! binary-only plugin lets a test observe. Against any other plugin those checks are skipped and
//! the run is the survival smoke test it has always been.
//!
//! This exists next to `tests/editor_open_tests.rs` because the same check as a `#[test]` needs
//! the harness to hand it the process main thread — AppKit refuses to create an `NSWindow`
//! anywhere else, and libtest always runs test functions on a spawned thread, `--test-threads=1`
//! included. An example binary owns `main`, so the constraint is met by construction; the test
//! runs this binary.

use std::sync::{Arc, Mutex};
use std::time::Duration;
use vst3_host::prelude::*;

/// Frames of host servicing to run while the editor is up, at roughly 60 Hz.
const FRAMES: u32 = 30;

// TestSynth's editor instrumentation. Ids and encodings are defined (and documented) in
// `test-plugin/src/lib.rs` under "Editor and state instrumentation"; every scale is a power of
// two, so decoding is exact.
const EDITOR_ATTACHED_PARAM_ID: u32 = 1000;
const EDITOR_WIDTH_PARAM_ID: u32 = 1001;
const EDITOR_HEIGHT_PARAM_ID: u32 = 1002;
const EDITOR_SCALE_PARAM_ID: u32 = 1003;
/// Each probe's id together with the exact title the plugin publishes it under.
const EDITOR_PROBES: [(u32, &str); 4] = [
    (EDITOR_ATTACHED_PARAM_ID, "Editor Attached"),
    (EDITOR_WIDTH_PARAM_ID, "Editor Width"),
    (EDITOR_HEIGHT_PARAM_ID, "Editor Height"),
    (EDITOR_SCALE_PARAM_ID, "Editor Scale"),
];
const EDITOR_SIZE_SCALE: f64 = 4096.0;
const EDITOR_SCALE_SCALE: f64 = 8.0;
/// The size TestSynth's view asks the host for, once, right after it is attached.
const EDITOR_SELF_RESIZE: (i32, i32) = (560, 400);

/// What an instrumented plugin's view recorded during the handshake.
struct EditorInstrumentation {
    attached: bool,
    last_on_size: (i32, i32),
    /// The content scale factor the host offered, or `0.0` if it never did.
    scale: f32,
}

fn main() {
    env_logger::init();
    std::process::exit(match run() {
        Ok(()) => {
            println!("EDITOR SMOKE OK");
            0
        }
        Err(message) => {
            eprintln!("EDITOR SMOKE FAILED: {message}");
            1
        }
    });
}

fn run() -> Result<(), String> {
    let path = std::env::args().nth(1).unwrap_or_else(default_plugin_path);
    println!("=== editor smoke: {path} ===");
    if !std::path::Path::new(&path).exists() {
        return Err(format!("plugin not found at {path}"));
    }

    let mut host = Vst3Host::builder()
        .sample_rate(48000.0)
        .block_size(512)
        .build()
        .map_err(|e| format!("build host: {e}"))?;
    let plugin = host
        .load_plugin(&path)
        .map_err(|e| format!("load {path}: {e}"))?;
    println!("load: {}", plugin.info().name);

    if !plugin.has_editor() {
        return Err("plugin reports no editor, nothing to smoke-test".to_string());
    }
    let size = plugin.get_editor_size();
    println!(
        "editor: reported size {size:?}, resizable={}",
        plugin.editor_can_resize()
    );

    let plugin = Arc::new(Mutex::new(plugin));
    let mut window = PluginWindow::new(plugin.clone());
    window.open().map_err(|e| format!("open editor: {e}"))?;
    if !window.is_open() {
        return Err("editor reported closed right after open()".to_string());
    }
    println!("open: editor window is up");

    for _ in 0..FRAMES {
        window
            .service_platform_events()
            .map_err(|e| format!("service editor window: {e}"))?;
        plugin
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .service_run_loop();
        std::thread::sleep(Duration::from_millis(16));
    }
    if !window.is_open() {
        return Err("editor window went away while it was being serviced".to_string());
    }
    println!("service: survived {FRAMES} frames");

    check_instrumentation(&plugin.lock().unwrap_or_else(|p| p.into_inner()))?;

    window.close();
    if window.is_open() {
        return Err("window still open after close()".to_string());
    }
    println!("close: editor detached and window destroyed");
    Ok(())
}

/// Verify the editor handshake against a plugin that reports what it saw.
///
/// A plugin without the instrumentation parameters (anything but our own TestSynth) is reported
/// and skipped — there is nothing to read, and the survival checks above already ran.
fn check_instrumentation(plugin: &Plugin) -> Result<(), String> {
    let Some(seen) = read_instrumentation(plugin) else {
        println!("instrumentation: none — plugin does not publish editor probes, skipping");
        return Ok(());
    };
    println!(
        "instrumentation: attached={} last onSize={}x{} content scale={:.2}",
        seen.attached, seen.last_on_size.0, seen.last_on_size.1, seen.scale
    );

    if !seen.attached {
        return Err("the plugin's view never saw IPlugView::attached".to_string());
    }
    if seen.last_on_size != EDITOR_SELF_RESIZE {
        return Err(format!(
            "the view asked the host to resize it to {}x{} and the host answered onSize {}x{} \
             — the resizeView -> container resize -> onSize chain is broken",
            EDITOR_SELF_RESIZE.0, EDITOR_SELF_RESIZE.1, seen.last_on_size.0, seen.last_on_size.1
        ));
    }
    // The window consumes the request while servicing frames. A leftover one means the host
    // recorded the resize but never applied it to the container.
    if let Some(pending) = plugin.take_editor_resize_request() {
        return Err(format!(
            "the host still has an unapplied resize request {pending:?} after servicing frames"
        ));
    }
    // macOS is the one platform where the host always offers a scale factor before attaching
    // (Windows offers it from the window's DPI, X11 has no host-side source for one).
    if cfg!(target_os = "macos") && seen.scale <= 0.0 {
        return Err("the host never offered the view a content scale factor".to_string());
    }
    println!("instrumentation: editor handshake verified end to end");
    Ok(())
}

/// Read the editor probes, or `None` when this plugin does not publish them.
fn read_instrumentation(plugin: &Plugin) -> Option<EditorInstrumentation> {
    let parameters = plugin.get_parameters().ok()?;
    // Match on id *and* title *and* the read-only flag. Plenty of plugins own id 1000 already —
    // Dexed's MIDI CC map does — and mistaking one of those for a probe would fail the run for
    // no reason at all.
    let published = EDITOR_PROBES.iter().all(|(id, title)| {
        parameters
            .iter()
            .any(|p| p.id == *id && p.name == *title && p.is_read_only)
    });
    if !published {
        return None;
    }
    let read = |id| plugin.get_parameter(id).unwrap_or(0.0);
    Some(EditorInstrumentation {
        attached: read(EDITOR_ATTACHED_PARAM_ID) >= 0.5,
        last_on_size: (
            (read(EDITOR_WIDTH_PARAM_ID) * EDITOR_SIZE_SCALE).round() as i32,
            (read(EDITOR_HEIGHT_PARAM_ID) * EDITOR_SIZE_SCALE).round() as i32,
        ),
        scale: (read(EDITOR_SCALE_PARAM_ID) * EDITOR_SCALE_SCALE) as f32,
    })
}

fn default_plugin_path() -> String {
    concat!(env!("CARGO_MANIFEST_DIR"), "/../test_plugins/Dexed.vst3").to_string()
}
