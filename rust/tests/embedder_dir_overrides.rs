//! Embedder contract guard for `core::paths::set_dir_overrides` (MILOU-137).
//!
//! Milou hosts lean-ctx as a library and must not manipulate the process
//! environment, so the layout is injected from code and has to be authoritative
//! for the whole process. This test pins the three guarantees the embedder
//! relies on:
//!
//! 1. every typed resolver answers inside the injected tree — `config_dir`,
//!    `data_dir`, `lean_ctx_data_dir`, `state_dir`, `cache_dir`, `runtime_dir`,
//!    plus the daemon's `daemon_pid_path` and the `daemon_addr` socket;
//! 2. the injection beats every competing environment variable
//!    (`LEAN_CTX_<CATEGORY>_DIR`, `XDG_*`, `LEAN_CTX_DATA_DIR`), so an
//!    embedder's run tree cannot be steered from outside the embedder. The
//!    back-compat half of that contract — a data-bearing `~/.lean-ctx` dragging
//!    all four categories onto one directory — is only reachable with
//!    `LEAN_CTX_*_DIR` unset, so it lives in its own process:
//!    `embedder_legacy_collapse.rs`;
//! 3. the installation is set-once: a second `set_dir_overrides` returns `false`
//!    and the first layout stays in force.
//!
//! Deliberately an integration test: it runs in its own process (so the
//! `OnceLock` starts fresh) and the library is compiled WITHOUT `#[cfg(test)]`,
//! so the production resolvers execute exactly as they do for an embedder — the
//! unit-test sandbox would otherwise short-circuit config/state/cache to a temp
//! dir and hide the behaviour.
//!
//! `daemon::disable_daemon_ipc()` is NOT armed here: this file proves that the
//! layout alone moves daemon discovery off the machine-wide daemon (see
//! `embedder_daemon_kill_switch.rs` for the switch itself).
//!
//! Hermetic: `HOME` and every XDG/`LEAN_CTX_*` variable point at a throwaway
//! tree and every write lands inside the injected temp tree, so a run can never
//! create or read anything under the developer's real `~/.lean-ctx`,
//! `~/.local/share/lean-ctx` or `~/Library/Application Support/lean-ctx`.

use std::ffi::OsString;
use std::path::Path;

use lean_ctx::core::data_dir::lean_ctx_data_dir;
use lean_ctx::core::paths::{
    DirOverrides, cache_dir, config_dir, data_dir, dir_overrides, runtime_dir, set_dir_overrides,
    state_dir,
};
use lean_ctx::daemon::{daemon_pid_path, is_daemon_running};

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

#[test]
fn injected_layout_wins_over_env_and_back_compat_and_is_set_once() {
    // Serializes every environment mutation in this binary.
    let _env_lock = lean_ctx::core::data_dir::test_env_lock();
    // Declared after the lock: dropping restores the environment while the lock
    // is still held.
    let _env_restore = EnvRestore::capture();

    let tmp = tempfile::tempdir().expect("temp dir for the injected layout");

    // ---- competing environment ------------------------------------------------
    // Every competitor the resolvers know about, each in its own directory so a
    // leak is attributable to one of them.
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
        std::fs::create_dir_all(dir).expect("create competing environment dir");
    }
    // A data-bearing `$HOME/.lean-ctx` is deliberately NOT created here: while
    // `LEAN_CTX_DATA_DIR` is set, `single_dir_override()` returns that pin before
    // the legacy probe runs, so the legacy branch cannot be reached from this
    // file — `embedder_legacy_collapse.rs` arms it explicitly instead.
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

    // ---- the embedder's injected layout --------------------------------------
    let run = tmp.path().join("run");
    let injected = DirOverrides {
        config: run.join("config"),
        data: run.join("data"),
        state: run.join("state"),
        cache: run.join("cache"),
    };
    for dir in [
        &injected.config,
        &injected.data,
        &injected.state,
        &injected.cache,
    ] {
        std::fs::create_dir_all(dir).expect("create injected category dir");
    }

    assert!(
        set_dir_overrides(injected.clone()),
        "the first installation must be accepted"
    );
    let installed = dir_overrides().expect("the injected layout is installed");
    assert_eq!(
        (
            installed.config.clone(),
            installed.data.clone(),
            installed.state.clone(),
            installed.cache.clone()
        ),
        (
            injected.config.clone(),
            injected.data.clone(),
            injected.state.clone(),
            injected.cache.clone()
        ),
        "dir_overrides() must report the injected layout verbatim"
    );

    // ---- every resolver answers from the injection ---------------------------
    // (label, actual, expected). `daemon_pid_path` is here rather than in the
    // cfg(unix) block because the pid file is derived from the state category on
    // every platform.
    let resolved = [
        (
            "config_dir",
            config_dir().expect("config_dir resolves"),
            injected.config.clone(),
        ),
        (
            "data_dir",
            data_dir().expect("data_dir resolves"),
            injected.data.clone(),
        ),
        (
            "lean_ctx_data_dir",
            lean_ctx_data_dir().expect("lean_ctx_data_dir resolves"),
            injected.data.clone(),
        ),
        (
            "state_dir",
            state_dir().expect("state_dir resolves"),
            injected.state.clone(),
        ),
        (
            "cache_dir",
            cache_dir().expect("cache_dir resolves"),
            injected.cache.clone(),
        ),
        (
            "runtime_dir",
            runtime_dir().expect("runtime_dir resolves"),
            injected.state.clone(),
        ),
        (
            "daemon_pid_path",
            daemon_pid_path(),
            injected.state.join("daemon.pid"),
        ),
    ];
    for (label, actual, expected) in &resolved {
        assert_eq!(
            actual, expected,
            "{label} must answer from the injected layout"
        );
        assert!(
            actual.starts_with(&run),
            "{label} must stay inside the injected tree, got {}",
            actual.display()
        );
    }

    // Restated negatively as a safety net. With the equalities above in force
    // this loop cannot fail on its own; it only fires if one of them is ever
    // weakened, and it then names the competitor that started winning.
    for (label, competitor) in [
        ("LEAN_CTX_CONFIG_DIR/XDG_CONFIG_HOME", &env_config),
        ("LEAN_CTX_DATA_DIR/XDG_DATA_HOME", &env_data),
        ("LEAN_CTX_STATE_DIR/XDG_STATE_HOME", &env_state),
        ("LEAN_CTX_CACHE_DIR/XDG_CACHE_HOME", &env_cache),
        ("XDG_RUNTIME_DIR", &env_runtime),
    ] {
        for (resolver, actual, _) in &resolved {
            assert!(
                !actual.starts_with(competitor),
                "{resolver} must not resolve under {label} ({})",
                competitor.display()
            );
        }
    }

    // ---- daemon discovery follows the injected STATE category -----------------
    // The discriminating assertion is the equality below: the pid file this
    // process would otherwise read is unreachable here (`HOME` is redirected, so
    // on macOS `dirs::data_local_dir()` resolves inside the temp tree), and only
    // the resolved path shows where discovery really looks. The control that
    // follows is self-confirming by construction — it writes its PID wherever
    // `daemon_pid_path()` points — so it only shows that the resolver reads that
    // tree rather than answering `false` unconditionally.
    let pid_path = daemon_pid_path();
    assert_eq!(pid_path, injected.state.join("daemon.pid"));
    // Positive control: a live PID written into the *injected* state dir is
    // discovered, which is what rules out an unconditional `false`.
    std::fs::write(&pid_path, std::process::id().to_string()).expect("write the pid file");
    assert!(
        is_daemon_running(),
        "a live PID under the injected state dir must be discovered"
    );
    std::fs::remove_file(&pid_path).expect("remove the pid file");
    assert!(
        !is_daemon_running(),
        "without a pid file under the injected state dir no daemon is running"
    );

    #[cfg(unix)]
    {
        let socket = match lean_ctx::daemon::daemon_addr() {
            lean_ctx::ipc::DaemonAddr::Unix(path) => path,
        };
        assert_eq!(
            socket,
            injected.state.join("daemon.sock"),
            "the daemon socket must follow the injected state dir"
        );
    }

    // ---- set-once ------------------------------------------------------------
    let refused = DirOverrides {
        config: tmp.path().join("second/config"),
        data: tmp.path().join("second/data"),
        state: tmp.path().join("second/state"),
        cache: tmp.path().join("second/cache"),
    };
    let refused_config = refused.config.clone();
    assert!(
        !set_dir_overrides(refused),
        "the layout is set-once: a second installation must be refused"
    );

    // The first layout is still the one in force, and the refused layout was
    // neither adopted nor created.
    assert_eq!(
        dir_overrides().map(|overrides| overrides.config.clone()),
        Some(injected.config.clone()),
        "the first layout stays installed"
    );
    assert_eq!(
        config_dir().expect("config_dir resolves"),
        injected.config,
        "resolvers keep answering from the first layout"
    );
    assert_ne!(
        config_dir().expect("config_dir resolves"),
        refused_config,
        "the refused layout must never be used"
    );
    assert!(
        !tmp.path().join("second").exists(),
        "the refused layout must not be materialized"
    );
}
