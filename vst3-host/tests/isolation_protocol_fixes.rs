//! Regression tests for the process-isolation transport: what the host does with anything
//! the other side of the boundary puts on the protocol stream, and how it behaves when that
//! side stops answering.
//!
//! The helper is stood in for by small shell scripts, so these run without a real plugin.

#![cfg(all(feature = "process-isolation", unix))]

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use vst3_host::process_isolation::{HostCommand, HostResponse, PluginHostProcess};

/// These tests spawn child processes, and one of them swaps the test process's own stdout and
/// stderr while it runs. Descriptors are process-global, so they take turns.
fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A throwaway executable standing in for the helper process.
struct FakeHelper {
    dir: PathBuf,
    path: PathBuf,
}

impl FakeHelper {
    fn new(name: &str, body: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("vst3_proto_{name}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("fake-helper");
        let mut f = std::fs::File::create(&path).expect("create fake helper");
        write!(f, "#!/bin/sh\n{body}").expect("write fake helper");
        drop(f);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        Self { dir, path }
    }

    fn spawn(&self, timeout: Duration) -> PluginHostProcess {
        PluginHostProcess::new(Some(self.path.clone()), timeout).expect("spawn fake helper")
    }
}

impl Drop for FakeHelper {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A plugin sharing the helper's stdout used to desynchronise the protocol permanently:
/// its `printf` was consumed as a response, and from then on every command was answered with
/// the previous command's reply. Unparseable lines are dropped instead, so replies keep
/// lining up with their requests.
#[test]
fn noise_on_the_protocol_stream_does_not_desync_replies() {
    let _serial = serial();
    let fake = FakeHelper::new(
        "noise",
        "n=0\n\
         while IFS= read -r line; do\n\
         \x20 printf 'plugin: hello from printf\\n'\n\
         \x20 printf 'not json either\\n'\n\
         \x20 printf '{\"ParameterValue\":{\"value\":%s}}\\n' \"$n\"\n\
         \x20 n=$((n+1))\n\
         done\n",
    );
    let mut helper = fake.spawn(Duration::from_secs(5));

    for i in 0..5u32 {
        match helper.send_command(HostCommand::GetParameter { id: i }) {
            Ok(HostResponse::ParameterValue { value }) => assert_eq!(
                value, i as f64,
                "reply {i} did not line up with its request"
            ),
            other => panic!("expected the reply to request {i}, got {other:?}"),
        }
    }
    assert!(
        helper.discarded_line_count() >= 10,
        "the noise lines should have been counted as discarded"
    );
}

/// A plugin dumping a large blob before the reply must not cost the host its place in the
/// stream: the blob is dropped as noise and the response that follows still arrives.
#[test]
fn a_large_noise_blob_does_not_break_the_stream() {
    let _serial = serial();
    let fake = FakeHelper::new(
        "flood",
        "while IFS= read -r line; do\n\
         \x20 dd if=/dev/zero bs=65536 count=8 2>/dev/null | tr '\\000' 'x'\n\
         \x20 printf '\\n'\n\
         \x20 printf '{\"Success\":{\"message\":\"ok\"}}\\n'\n\
         done\n",
    );
    let mut helper = fake.spawn(Duration::from_secs(20));

    for _ in 0..2 {
        match helper.send_command(HostCommand::StartProcessing) {
            Ok(HostResponse::Success { .. }) => {}
            other => panic!("expected Success after the flood, got {other:?}"),
        }
    }
}

/// A helper that dies while writing its response leaves a truncated line behind. That is a
/// crash, and must be reported as one — not as a parse failure that leaves `is_alive()`
/// claiming the helper is fine.
#[test]
fn a_helper_dying_mid_response_is_reported_as_a_crash() {
    let _serial = serial();
    let fake = FakeHelper::new(
        "truncated",
        "read -r line\n\
         printf '{\"Success\":{\"mess'\n\
         kill -9 $$\n",
    );
    let mut helper = fake.spawn(Duration::from_secs(5));

    let err = helper
        .send_command(HostCommand::StartProcessing)
        .expect_err("a truncated response must be an error");
    assert!(
        err.to_lowercase().contains("crash") || err.contains("exited"),
        "a helper that died mid-write must be classified as a crash, got: {err}"
    );
    assert!(
        !helper.is_alive(),
        "the helper must not be reported as alive after it died mid-response"
    );
}

/// `shutdown` runs from `Drop`, so it must stay bounded even when the helper's stdout pipe is
/// held open by something the host does not control — a plugin-spawned grandchild, say.
#[test]
fn shutdown_does_not_wait_for_a_grandchild_holding_the_pipe() {
    let _serial = serial();
    let fake = FakeHelper::new(
        "grandchild",
        // The grandchild inherits stdout and outlives the helper by a long way.
        "sleep 30 &\nexit 0\n",
    );
    let mut helper = fake.spawn(Duration::from_secs(5));

    let started = Instant::now();
    helper.shutdown();
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "shutdown must not wait on a pipe a grandchild holds open, took {elapsed:?}"
    );
}

/// Loading a plugin or serializing its state is legitimately slow; the per-block deadline
/// exists for a plugin hung inside `process()`. The two must not share one number.
#[test]
fn load_and_state_commands_get_their_own_deadline() {
    let _serial = serial();
    let fake = FakeHelper::new("deadlines", "exec sleep 30\n");

    let mut fast = fake.spawn(Duration::from_millis(150));
    fast.set_slow_command_timeout(Duration::from_millis(1500));
    let started = Instant::now();
    let _ = fast.send_command(HostCommand::Process {
        inputs: vec![vec![0.0; 64]],
        frames: 64,
    });
    let block_wait = started.elapsed();
    assert!(
        block_wait < Duration::from_millis(900),
        "process() must keep the short deadline, waited {block_wait:?}"
    );

    let mut slow = fake.spawn(Duration::from_millis(150));
    slow.set_slow_command_timeout(Duration::from_millis(1500));
    let started = Instant::now();
    let _ = slow.send_command(HostCommand::SaveState);
    let state_wait = started.elapsed();
    assert!(
        state_wait >= Duration::from_millis(1400),
        "SaveState must get the longer deadline, waited {state_wait:?}"
    );
}

/// The helper's protocol channel must be out of reach of the plugin it loads: after
/// `ProtocolChannel::claim`, writes to file descriptor 1 go to stderr and only protocol
/// writes reach the host.
///
/// Runs in-process (the helper does this to itself at startup), so it swaps the test
/// process's own stdout/stderr for the duration and restores them before returning.
#[test]
fn claiming_the_protocol_channel_takes_stdout_away_from_the_plugin() {
    use std::io::Read;
    use vst3_host::process_isolation::ProtocolChannel;

    let _serial = serial();

    fn pipe() -> (i32, i32) {
        let mut fds = [0i32; 2];
        // SAFETY: `pipe` fills the two-element array it is given with fresh descriptors.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe");
        (fds[0], fds[1])
    }
    fn read_all(fd: i32) -> String {
        use std::os::fd::FromRawFd;
        // SAFETY: `fd` is a read end this test owns and hands over to the `File`.
        let mut f = unsafe { std::fs::File::from_raw_fd(fd) };
        let mut s = String::new();
        let _ = f.read_to_string(&mut s);
        s
    }

    let (protocol_r, protocol_w) = pipe();
    let (stderr_r, stderr_w) = pipe();

    // SAFETY: all four descriptors are owned by this test; the originals are duplicated
    // first and restored below, so the process is left exactly as it was found.
    let (saved_out, saved_err) = unsafe {
        let saved = (libc::dup(1), libc::dup(2));
        libc::dup2(protocol_w, 1);
        libc::dup2(stderr_w, 2);
        libc::close(protocol_w);
        libc::close(stderr_w);
        saved
    };

    let mut channel = ProtocolChannel::claim();
    let response = "{\"Success\":{\"message\":\"ok\"}}";
    let _ = writeln!(channel, "{response}");
    let _ = channel.flush();
    // What a third-party plugin does: write straight to stdout.
    let noise = b"plugin: chatty printf\n";
    // SAFETY: a plain write to file descriptor 1 from a buffer that outlives the call.
    unsafe { libc::write(1, noise.as_ptr().cast(), noise.len()) };
    drop(channel);

    // SAFETY: restore the saved descriptors and release the duplicates.
    unsafe {
        libc::dup2(saved_out, 1);
        libc::dup2(saved_err, 2);
        libc::close(saved_out);
        libc::close(saved_err);
    }

    let protocol_text = read_all(protocol_r);
    let stderr_text = read_all(stderr_r);

    assert!(
        protocol_text.contains(response),
        "the response must reach the host, got {protocol_text:?}"
    );
    assert!(
        !protocol_text.contains("chatty printf"),
        "plugin stdout must never enter the protocol stream, got {protocol_text:?}"
    );
    assert!(
        stderr_text.contains("chatty printf"),
        "plugin stdout should be merged into stderr, got {stderr_text:?}"
    );
}
