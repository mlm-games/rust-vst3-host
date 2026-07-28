//! Editor-open regression coverage against the bundled plugins.
//!
//! `PluginWindow::open()` is the one path that exercises the whole native-editor handshake:
//! `has_editor` → create the OS window → `IPlugView::setFrame`/`attached` →
//! `setContentScaleFactor`. A regression anywhere in it (for instance a plugin *declining*
//! content-scale support being treated as a hard error) makes every JUCE-based editor fail to
//! open — which no other test in the suite would notice, because nothing else opens an editor.
//!
//! Two plugins, two different jobs:
//!
//! - **Dexed** is a real third-party (JUCE) editor. It proves the path works against a plugin
//!   nobody here wrote, but it can only be observed from the outside: did it open, did it stay
//!   up, did it close.
//! - **TestSynth** is the in-repo fixture. Its view records every step of the handshake and
//!   republishes it as read-only parameters, so this run asserts the *inside* of the protocol:
//!   the view was attached, it was offered a content scale, and its `IPlugFrame::resizeView`
//!   request came back as an `onSize` of exactly the size it asked for. It is also the only one
//!   of the two that exists on Linux and Windows.
//!
//! ## Why these tests shell out
//!
//! AppKit refuses to create an `NSWindow` off the process main thread, and libtest always runs
//! test functions on a spawned thread — `--test-threads=1` included. So the check itself lives
//! in `examples/editor_smoke.rs`, which owns `main` and therefore the main thread; these tests
//! run that binary and assert it succeeds. `cargo test` builds examples, so the binary is
//! already sitting next to this test executable.
//!
//! ```bash
//! cargo test -p vst3-host --test editor_open_tests -- --ignored --nocapture
//! cargo run  -p vst3-host --example editor_smoke                   # the same check, directly
//! ```
//!
//! `#[ignore]`d like the other real-plugin tests: they need the bundled plugins and briefly put
//! a window on screen.

use std::path::PathBuf;
use std::process::{Command, Output};

/// Path to a bundled test plugin; `None` (with a printed note) if it is missing, so an
/// `--ignored` run on a machine without it degrades gracefully.
fn bundled_plugin(name: &str) -> Option<PathBuf> {
    let path = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../test_plugins")).join(name);
    if path.exists() {
        Some(path)
    } else {
        println!("Test plugin not found at {}, skipping", path.display());
        None
    }
}

/// The `editor_smoke` example cargo built alongside this test: `target/<profile>/deps/<test>`
/// sits one directory below `target/<profile>/examples/`.
fn editor_smoke_binary() -> PathBuf {
    let test_exe = std::env::current_exe().expect("locate the test executable");
    let profile_dir = test_exe
        .parent()
        .and_then(|deps| deps.parent())
        .expect("test executable lives in target/<profile>/deps");
    profile_dir
        .join("examples")
        .join(format!("editor_smoke{}", std::env::consts::EXE_SUFFIX))
}

/// Run the smoke binary against one plugin and assert it reported success.
fn run_editor_smoke(plugin: &PathBuf) -> String {
    let binary = editor_smoke_binary();
    assert!(
        binary.exists(),
        "{} is missing — build it with `cargo build -p vst3-host --example editor_smoke`",
        binary.display()
    );

    let Output {
        status,
        stdout,
        stderr,
    } = Command::new(&binary)
        .arg(plugin)
        .output()
        .unwrap_or_else(|e| panic!("running {}: {e}", binary.display()));
    let stdout = String::from_utf8_lossy(&stdout).into_owned();
    let stderr = String::from_utf8_lossy(&stderr);
    println!("{stdout}");

    assert!(
        status.success(),
        "opening {}'s editor failed (exit {:?})\nstdout:\n{stdout}\nstderr:\n{stderr}",
        plugin.display(),
        status.code()
    );
    assert!(
        stdout.contains("EDITOR SMOKE OK"),
        "the smoke run exited 0 without reporting success\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    stdout
}

#[test]
#[ignore = "opens a real editor window; needs test_plugins/Dexed.vst3"]
fn dexed_editor_opens_services_and_closes() {
    let Some(plugin) = bundled_plugin("Dexed.vst3") else {
        return;
    };
    run_editor_smoke(&plugin);
}

/// The same run against our own fixture, which can prove what actually crossed the boundary.
#[test]
#[ignore = "opens a real editor window; needs test_plugins/TestSynth.vst3 (just test-plugin)"]
fn testsynth_editor_handshake_is_verified_end_to_end() {
    let Some(plugin) = bundled_plugin("TestSynth.vst3") else {
        return;
    };
    let stdout = run_editor_smoke(&plugin);

    // The smoke binary fails the run itself if any of this is wrong; asserting on its report
    // here is what stops the checks from silently going missing (a plugin that stopped
    // publishing the probes would just be "skipped" and the run would still be green).
    assert!(
        stdout.contains("instrumentation: attached=true last onSize=560x400"),
        "the view was not attached, or the host's resizeView -> onSize chain did not deliver \
         the 560x400 the view asked for\nstdout:\n{stdout}"
    );
    assert!(
        stdout.contains("instrumentation: editor handshake verified end to end"),
        "the smoke run skipped the instrumented checks instead of running them\nstdout:\n{stdout}"
    );
    if cfg!(target_os = "macos") {
        assert!(
            stdout.contains("content scale=1.00"),
            "macOS must offer the view a content scale factor before attaching\nstdout:\n{stdout}"
        );
    }
}
