//! Embedder contract guard for `daemon::disable_daemon_ipc` (MILOU-136).
//!
//! Milou embeds lean-ctx as a library and must never talk to the machine-wide
//! daemon. This test pins the three directions in which the one-way switch has
//! to behave exactly like "no daemon on this machine":
//!
//! 1. `is_daemon_running()` answers `false`, so every connect-only path
//!    (delivery, cross-agent cache, index nudge) skips before the socket;
//! 2. `daemon_client::daemon_request` refuses *before* any connect/write/read —
//!    proven against an endpoint that is discoverable and carries a bound
//!    listener, so a refusal that merely failed to find the daemon would not
//!    pass;
//! 3. `start_daemon()` refuses before `acquire_start_lock()`, so the auto-start
//!    path can never spawn a daemon.
//!
//! The test arms the injected layout first (`core::paths::set_dir_overrides`),
//! exactly as the embedder does, which makes the pid file and the socket
//! hermetic: both land in a temp state dir, so the assertions can neither be
//! satisfied by nor interfere with a real machine-wide daemon.
//!
//! The *environment* is redirected into the same temp tree as well. The
//! injected layout is process-local, while a daemon spawned by `start_daemon`
//! is a child process that resolves its directories from `HOME`/`XDG_*`/
//! `LEAN_CTX_*`. Should the kill-switch ever regress, the child would inherit
//! the real environment and could touch the real daemon's pid file and socket
//! before the test gets to fail; redirecting those variables keeps even that
//! failure inside the temp tree.
//!
//! Own binary on purpose: the injected layout and the kill-switch are both
//! process-global and one-way, which is exactly the exception `tests/suite/`
//! documents for standalone `tests/*.rs` targets. The pure-layout guard (no
//! kill-switch armed) is `embedder_dir_overrides.rs`; the legacy-collapse guard
//! is `embedder_legacy_collapse.rs`.

use std::ffi::OsString;
use std::path::Path;
use std::time::{Duration, Instant};

use lean_ctx::core::paths::{DirOverrides, set_dir_overrides};
use lean_ctx::daemon::{
    daemon_ipc_disabled, daemon_pid_path, disable_daemon_ipc, is_daemon_running, start_daemon,
};
use lean_ctx::daemon_client::daemon_request;

/// The longest a refusal may take. The switch answers by branching, so any
/// measurable delay here would mean the call reached the socket; under the
/// switch the endpoint below is live and would block on connect/IO instead.
const REFUSAL_BUDGET: Duration = Duration::from_secs(1);

/// Environment variables this test owns; restored on drop, even on a panic, so a
/// failure cannot leak a redirected `HOME` into the rest of this process.
const MANAGED_ENV: &[&str] = &[
    "HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_CACHE_HOME",
    "XDG_RUNTIME_DIR",
    "LEAN_CTX_DATA_DIR",
    "LEAN_CTX_CONFIG_DIR",
    "LEAN_CTX_STATE_DIR",
    "LEAN_CTX_CACHE_DIR",
];

/// Sets a directory environment variable (test-only).
///
/// Must only be called while the caller holds `test_env_lock()`: `set_var` is
/// unsound if another thread reads the environment concurrently.
fn set_env(key: &str, value: &Path) {
    // SAFETY: every caller holds `test_env_lock()` for the whole test body, so
    // no other thread reads or writes the environment concurrently.
    unsafe { std::env::set_var(key, value) };
}

/// Removes an environment variable (test-only).
///
/// Must only be called while the caller holds `test_env_lock()`.
fn remove_env(key: &str) {
    // SAFETY: the caller holds `test_env_lock()` for the whole test body, so no
    // other thread reads or writes the environment concurrently.
    unsafe { std::env::remove_var(key) };
}

/// Restores [`MANAGED_ENV`] to the values it had when the guard was created.
struct EnvRestore {
    saved: Vec<(&'static str, Option<OsString>)>,
}

impl EnvRestore {
    fn capture() -> Self {
        Self {
            saved: MANAGED_ENV
                .iter()
                .map(|key| (*key, std::env::var_os(key)))
                .collect(),
        }
    }
}

impl Drop for EnvRestore {
    fn drop(&mut self) {
        for (key, value) in &self.saved {
            match value {
                Some(value) => set_env(key, Path::new(value)),
                None => remove_env(key),
            }
        }
    }
}

#[tokio::test]
async fn kill_switch_disarms_discovery_requests_and_autostart() {
    // Serializes every environment mutation in this binary.
    let _env_lock = lean_ctx::core::data_dir::test_env_lock();
    // Declared after the lock: dropping restores the environment while the lock
    // is still held.
    let _env_restore = EnvRestore::capture();

    let tmp = tempfile::tempdir().expect("temp dir for the kill-switch guard");

    // Redirect the environment first, so nothing below — including a daemon a
    // regression might spawn — can resolve a path outside `tmp`.
    let env_home = tmp.path().join("env-home");
    let env_config = tmp.path().join("env-config");
    let env_data = tmp.path().join("env-data");
    let env_state = tmp.path().join("env-state");
    let env_cache = tmp.path().join("env-cache");
    let env_runtime = tmp.path().join("env-runtime");
    for dir in [
        &env_home,
        &env_config,
        &env_data,
        &env_state,
        &env_cache,
        &env_runtime,
    ] {
        std::fs::create_dir_all(dir).expect("create redirected environment dir");
    }
    set_env("HOME", &env_home);
    for (key, dir) in [
        ("XDG_CONFIG_HOME", &env_config),
        ("XDG_DATA_HOME", &env_data),
        ("XDG_STATE_HOME", &env_state),
        ("XDG_CACHE_HOME", &env_cache),
        ("XDG_RUNTIME_DIR", &env_runtime),
        ("LEAN_CTX_CONFIG_DIR", &env_config),
        ("LEAN_CTX_DATA_DIR", &env_data),
        ("LEAN_CTX_STATE_DIR", &env_state),
        ("LEAN_CTX_CACHE_DIR", &env_cache),
    ] {
        set_env(key, dir);
    }

    let config = tmp.path().join("config");
    let data = tmp.path().join("data");
    let state = tmp.path().join("state");
    let cache = tmp.path().join("cache");
    for dir in [&config, &data, &state, &cache] {
        std::fs::create_dir_all(dir).expect("create injected category dir");
    }

    // Embedder layout: every daemon path below resolves inside `tmp`, and the
    // injected categories win over the redirects above.
    assert!(
        set_dir_overrides(DirOverrides {
            config,
            data,
            state: state.clone(),
            cache,
        }),
        "the injected layout must be accepted"
    );

    let pid_path = daemon_pid_path();
    assert_eq!(
        pid_path,
        state.join("daemon.pid"),
        "the pid file must follow the injected state dir"
    );

    // A daemon that *looks* alive: a live PID in the run's pid file plus a bound
    // listener at the endpoint `daemon_request` would contact.
    std::fs::write(&pid_path, std::process::id().to_string()).expect("write the pid file");

    #[cfg(unix)]
    let _listener = {
        let socket = match lean_ctx::daemon::daemon_addr() {
            lean_ctx::ipc::DaemonAddr::Unix(path) => path,
        };
        assert_eq!(
            socket,
            state.join("daemon.sock"),
            "the socket must follow the injected state dir"
        );
        // The listener is bound by the test itself and never accepts: that is a
        // separate fact from the check below.
        tokio::net::UnixListener::bind(&socket).expect("bind a listener at the run's socket")
    };

    // Controls: unarmed, discovery already answers from the run's pid file and
    // the endpoint is discoverable, so the post-arm assertions cannot pass
    // vacuously. (`DaemonAddr::is_listening` is a socket-file existence check,
    // not a connection probe — the bound listener above is what makes the
    // endpoint genuinely reachable.)
    assert!(
        !daemon_ipc_disabled(),
        "the switch must start disarmed in this test"
    );
    assert!(
        is_daemon_running(),
        "control: a live PID under the run's state dir is discovered"
    );
    #[cfg(unix)]
    assert!(
        lean_ctx::daemon::daemon_addr().is_listening(),
        "control: the run's socket file exists"
    );

    // ---- arm the switch ------------------------------------------------------
    disable_daemon_ipc();
    assert!(daemon_ipc_disabled(), "the switch must report itself armed");
    assert!(
        !is_daemon_running(),
        "discovery must answer 'not running' despite a live PID file"
    );

    // `daemon_request` refuses by branching on the switch. The endpoint is
    // discoverable and listening, so a call that got past the check would block
    // on connect/IO and blow the budget — the point is that no network
    // interaction happens at all.
    let started = Instant::now();
    let refused = daemon_request("GET", "/health", "").await;
    let elapsed = started.elapsed();
    let error = refused.expect_err("daemon_request must refuse while the switch is armed");
    assert!(
        error
            .to_string()
            .contains("daemon IPC disabled by embedder"),
        "unexpected refusal: {error:#}"
    );
    assert!(
        elapsed < REFUSAL_BUDGET,
        "the refusal must precede any connect/IO, took {elapsed:?}"
    );

    // `start_daemon` refuses before `acquire_start_lock()`, so nothing can be
    // spawned: the lock file the critical section would have created is absent.
    let error = start_daemon(&[]).expect_err("start_daemon must refuse while the switch is armed");
    assert!(
        error.to_string().contains("refusing to start a daemon"),
        "unexpected refusal: {error:#}"
    );
    assert!(
        !state.join("daemon.start.lock").exists(),
        "the refusal must precede acquire_start_lock()"
    );

    // Idempotent: a one-way AtomicBool, safe to call repeatedly, and the state it
    // enforces never flips back.
    for _ in 0..3 {
        disable_daemon_ipc();
    }
    assert!(daemon_ipc_disabled(), "the switch stays armed");
    assert!(!is_daemon_running(), "discovery stays disarmed");
    assert!(
        daemon_request("GET", "/health", "").await.is_err(),
        "requests stay refused"
    );
}
