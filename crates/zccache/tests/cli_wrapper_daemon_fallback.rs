//! The wrapper must never turn a daemon fault into a build failure.
//!
//! `zccache` is wired in as cargo's `rustc-wrapper`, so every rustc invocation
//! in the build goes through it. That makes one property load-bearing: wrapping
//! a compiler must not change whether the build succeeds. When the daemon is
//! unreachable, wedged, crashed, or talking a protocol the client can't parse,
//! it has produced no verdict about the compile — and the only honest answer is
//! to run the compiler and report what it actually did.
//!
//! Pre-fix, every one of those paths returned `ExitCode::FAILURE` without ever
//! running the compiler, so cargo reported
//!
//! ```text
//! error: could not compile `naga` (lib)
//! Caused by: process didn't exit successfully: `zccache rustc --crate-name naga ...` (exit status: 1)
//! ```
//!
//! for a crate that compiles cleanly with `RUSTC_WRAPPER=""`. No `cargo clean`
//! could clear it: the fault was in the wrapper, not in `target/`.
//!
//! These tests need no daemon — `ZCCACHE_NO_SPAWN=1` plus an isolated cache
//! root makes the daemon deterministically unavailable, which is exactly the
//! "client cannot get a verdict" condition. `echo_shim` stands in for the
//! compiler: it proves the tool really ran (markers), that its exit code is
//! reported verbatim, and that piped stdin is replayed rather than swallowed.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

use std::io::Write;
use std::process::{Command, Stdio};

const STDOUT_MARKER: &[u8] = b"ZCCACHE_PASSTHROUGH_STDOUT_MARKER\n";
const STDERR_MARKER: &[u8] = b"ZCCACHE_PASSTHROUGH_STDERR_MARKER\n";

fn binary_path(stem: &str) -> std::path::PathBuf {
    // Test binaries live at target/<profile>/deps/<name>-<hash>; the profile
    // dir two levels up holds the crate's bins.
    let mut p = std::env::current_exe().expect("current_exe");
    p.pop(); // deps/
    p.pop(); // target/<profile>/
    if cfg!(windows) {
        p.push(format!("{stem}.exe"));
    } else {
        p.push(stem);
    }
    p
}

/// Run `zccache <echo_shim> <exit_code>` against a deliberately unavailable
/// daemon, returning the wrapper's (exit code, stdout, stderr).
fn run_with_daemon_unavailable(
    stdin_payload: &[u8],
    exit_code: i32,
    session_id: Option<&str>,
) -> (i32, Vec<u8>, Vec<u8>) {
    let zccache = binary_path("zccache");
    let echo_shim = binary_path("echo_shim");
    assert!(
        zccache.exists(),
        "zccache binary missing at {zccache:?} — build with --features zccache-bin"
    );
    assert!(
        echo_shim.exists(),
        "echo_shim binary missing at {echo_shim:?} — build with --features test-support"
    );
    let cache_dir = tempfile::Builder::new()
        .prefix("zccache-daemon-fallback-")
        .tempdir()
        .expect("tempdir");

    let mut cmd = Command::new(&zccache);
    cmd.arg(&echo_shim)
        .arg(exit_code.to_string())
        .env("ZCCACHE_CACHE_DIR", cache_dir.path())
        .env(
            "ZCCACHE_DAEMON_NAMESPACE",
            format!("fallback-{}", std::process::id()),
        )
        // The daemon cannot be started, so the client cannot obtain a verdict:
        // the same condition a crashed or unreachable daemon produces.
        .env("ZCCACHE_NO_SPAWN", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match session_id {
        Some(sid) => {
            cmd.env("ZCCACHE_SESSION_ID", sid);
        }
        None => {
            cmd.env_remove("ZCCACHE_SESSION_ID");
        }
    }

    let mut child = cmd.spawn().expect("spawn zccache wrapper");
    {
        let mut stdin = child.stdin.take().expect("piped stdin");
        stdin.write_all(stdin_payload).expect("write stdin payload");
    }
    let output = child.wait_with_output().expect("wait_with_output");
    (
        output.status.code().unwrap_or(-1),
        output.stdout,
        output.stderr,
    )
}

fn assert_tool_actually_ran(stdout: &[u8], stderr: &[u8]) {
    assert!(
        stdout
            .windows(STDOUT_MARKER.len())
            .any(|w| w == STDOUT_MARKER),
        "the wrapper never ran the tool: STDOUT_MARKER missing. \
         A daemon fault must fall back to running the real compiler, not exit 1.\n\
         stdout = {:?}\nstderr = {:?}",
        String::from_utf8_lossy(stdout),
        String::from_utf8_lossy(stderr)
    );
    assert!(
        stderr
            .windows(STDERR_MARKER.len())
            .any(|w| w == STDERR_MARKER),
        "STDERR_MARKER missing from wrapper stderr.\nstderr = {:?}",
        String::from_utf8_lossy(stderr)
    );
}

/// The naga case: cargo's rustc-wrapper route (no `ZCCACHE_SESSION_ID`, so
/// `cmd_compile_ephemeral`). An unavailable daemon must not fail the build.
#[test]
fn daemon_unavailable_still_runs_the_compiler() {
    let (code, stdout, stderr) = run_with_daemon_unavailable(b"", 0, None);
    assert_tool_actually_ran(&stdout, &stderr);
    assert_eq!(
        code,
        0,
        "a clean compile must stay clean when the daemon is unavailable.\nstderr = {:?}",
        String::from_utf8_lossy(&stderr)
    );
}

/// The fallback must not paper over real failures: the tool's own non-zero
/// exit code is the build's answer and must be reported verbatim.
#[test]
fn daemon_unavailable_preserves_the_tools_exit_code() {
    let (code, stdout, stderr) = run_with_daemon_unavailable(b"", 3, None);
    assert_tool_actually_ran(&stdout, &stderr);
    assert_eq!(
        code, 3,
        "the tool exited 3; the wrapper must report 3, not 0 and not 1"
    );
}

/// The session route (`cmd_compile`, used when a host sets
/// `ZCCACHE_SESSION_ID`) reaches the daemon through a different function and
/// must honour the same contract.
#[test]
fn daemon_unavailable_still_runs_the_compiler_on_the_session_route() {
    let (code, stdout, stderr) = run_with_daemon_unavailable(b"", 0, Some("fallback-session"));
    assert_tool_actually_ran(&stdout, &stderr);
    assert_eq!(
        code,
        0,
        "the session route must fall back too.\nstderr = {:?}",
        String::from_utf8_lossy(&stderr)
    );
}

/// The wrapper slurps its own stdin before talking to the daemon. On the
/// fallback path those bytes must be replayed to the child, or a compiler
/// reading from stdin would see an empty input and silently produce the
/// wrong artifact.
#[test]
fn daemon_unavailable_replays_piped_stdin_to_the_compiler() {
    let payload: Vec<u8> = (0..=255u8).cycle().take(4096).collect();
    let (code, stdout, stderr) = run_with_daemon_unavailable(&payload, 0, None);
    assert_tool_actually_ran(&stdout, &stderr);
    assert_eq!(code, 0);

    let marker_end = stderr
        .windows(STDERR_MARKER.len())
        .position(|w| w == STDERR_MARKER)
        .expect("STDERR_MARKER present")
        + STDERR_MARKER.len();
    assert!(
        stderr[marker_end..]
            .windows(payload.len())
            .any(|w| w == payload.as_slice()),
        "the {} bytes piped into the wrapper never reached the tool — \
         the fallback must replay slurped stdin, not hand the child an exhausted pipe",
        payload.len()
    );
}
