//! Embedder contract guard for the **legacy/mixed-install** half of
//! `core::paths::set_dir_overrides` (MILOU-137).
//!
//! `embedder_dir_overrides.rs` covers the injection against the environment
//! variables, but it sets `LEAN_CTX_DATA_DIR`, which short-circuits
//! `single_dir_override()` before the legacy branch is ever reached. The
//! back-compat collapse — a data-bearing `$HOME/.lean-ctx` (or a pre-split
//! `$XDG_CONFIG_HOME/lean-ctx`) dragging all four categories onto one directory
//! — is therefore exercised here, with **no** `LEAN_CTX_*_DIR` and no category
//! pin of any kind.
//!
//! The test first asserts the collapse is genuinely armed (without the
//! injection the four categories resolve onto `$HOME/.lean-ctx`), then installs
//! the layout and asserts that every resolver answers from the injected tree
//! and none of them from the legacy directory. Reverting the injection hook
//! makes it fail on either half.
//!
//! Hermetic: `HOME` and every `XDG_*` variable point at a throwaway tree, and
//! the legacy install it arms lives inside it — the developer's real
//! `~/.lean-ctx`, `~/.local/share/lean-ctx` and `~/Library/Application
//! Support/lean-ctx` are never read or written.
//!
//! Own binary on purpose: `set_dir_overrides` is a process-global `OnceLock`,
//! which is the exception `tests/suite/` documents for standalone `tests/*.rs`
//! targets. Compiles WITHOUT `#[cfg(test)]`, so the production resolvers run.

use std::ffi::OsString;
use std::path::Path;

use lean_ctx::core::data_dir::lean_ctx_data_dir;
use lean_ctx::core::paths::{
    DirOverrides, cache_dir, config_dir, data_dir, runtime_dir, set_dir_overrides, state_dir,
};
use lean_ctx::daemon::daemon_pid_path;

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
fn injected_layout_beats_the_legacy_single_dir_collapse() {
    // Serializes every environment mutation in this binary.
    let _env_lock = lean_ctx::core::data_dir::test_env_lock();
    // Declared after the lock: dropping restores the environment while the lock
    // is still held.
    let _env_restore = EnvRestore::capture();

    let tmp = tempfile::tempdir().expect("temp dir for the legacy-collapse guard");

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

    // A legacy single-dir install: `$HOME/.lean-ctx` holding real data (a
    // non-empty `stats.json` marker). This is what `single_dir_override_fs`
    // detects, and what would collapse every category onto it.
    let legacy = env_home.join(".lean-ctx");
    std::fs::create_dir_all(&legacy).expect("create the legacy install dir");
    std::fs::write(legacy.join("stats.json"), "{}").expect("write the legacy data marker");

    set_env("HOME", &env_home);
    for (key, dir) in [
        ("XDG_CONFIG_HOME", &env_config),
        ("XDG_DATA_HOME", &env_data),
        ("XDG_STATE_HOME", &env_state),
        ("XDG_CACHE_HOME", &env_cache),
        ("XDG_RUNTIME_DIR", &env_runtime),
    ] {
        set_env(key, dir);
    }
    // No category pin: any `LEAN_CTX_*_DIR` would short-circuit the legacy
    // branch this test exists to exercise.
    for key in [
        "LEAN_CTX_DATA_DIR",
        "LEAN_CTX_CONFIG_DIR",
        "LEAN_CTX_STATE_DIR",
        "LEAN_CTX_CACHE_DIR",
    ] {
        remove_env(key);
    }

    // ---- control: the collapse is armed --------------------------------------
    // Untouched by any injection, all four categories resolve onto the legacy
    // install in the temp home — so the post-injection assertions below cannot
    // pass by simply having no legacy install to escape.
    //
    // `runtime_dir()` is deliberately absent: `XDG_RUNTIME_DIR` (redirected to
    // the temp tree above) short-circuits it before the collapse runs. That is
    // upstream behaviour and not what this guard is about.
    assert_eq!(
        config_dir().expect("config_dir resolves"),
        legacy,
        "control: a data-bearing ~/.lean-ctx collapses config onto itself"
    );
    assert_eq!(
        data_dir().expect("data_dir resolves"),
        legacy,
        "control: a data-bearing ~/.lean-ctx collapses data onto itself"
    );
    assert_eq!(
        state_dir().expect("state_dir resolves"),
        legacy,
        "control: a data-bearing ~/.lean-ctx collapses state onto itself"
    );
    assert_eq!(
        cache_dir().expect("cache_dir resolves"),
        legacy,
        "control: a data-bearing ~/.lean-ctx collapses cache onto itself"
    );

    // ---- install the embedder layout -----------------------------------------
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
        "the injected layout must be accepted"
    );

    // ---- the injection wins over the collapse --------------------------------
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
        assert!(
            !actual.starts_with(&legacy),
            "{label} must not preserve the legacy collapse onto ~/.lean-ctx (got {})",
            actual.display()
        );
    }
}
