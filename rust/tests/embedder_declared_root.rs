//! Embedder contract guard for `core::pathutil::declare_project_root` (MILOU-297).
//!
//! Milou opens an engine on the directory an ACP client gives as the session's
//! working directory, and that directory may carry no project marker: an empty
//! folder, a notes directory, Milou's own home. Without a marker the server
//! treats such a root as a stray working directory, in two places:
//!
//! 1. the path jail re-roots the session to the first marked project an
//!    absolute path reaches (`tools/server_paths.rs`,
//!    `maybe_reroot_for_absolute_path`), so a read of a file in a neighbouring
//!    project succeeds and the jail no longer holds;
//! 2. the scan guard refuses a direct child of the home directory
//!    (`core/graph_index`, `is_safe_scan_root`), which turns off the index and
//!    the walks of `ctx_search`, `ctx_tree` and `ctx_glob`.
//!
//! The test proves both against a control taken before the declaration, so it
//! cannot pass vacuously, and then shows that a declared root holds the jail
//! and is scanned, while a declared broad root stays broad.
//!
//! The environment (`HOME` and the directory variables) and the layout are
//! redirected into a temp tree first: the scan guard reads the home directory
//! from `HOME`, and the engine's own state must not land in a real directory.
//!
//! Own binary on purpose: the declarations and the injected layout are
//! process-global and one-way, which is the exception `tests/suite/` documents
//! for standalone `tests/*.rs` targets.

use std::ffi::OsString;
use std::path::Path;

use lean_ctx::core::graph_index::is_safe_scan_root_public;
use lean_ctx::core::paths::{DirOverrides, set_dir_overrides};
use lean_ctx::core::pathutil::{declare_project_root, has_project_marker, is_broad_or_unsafe_root};
use lean_ctx::daemon::disable_daemon_ipc;
use lean_ctx::engine::ContextEngine;
use serde_json::{Value, json};

/// Environment variables this test owns; restored on drop, even on a panic.
const MANAGED_ENV: &[&str] = &[
    "HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_CACHE_HOME",
    "LEAN_CTX_DATA_DIR",
    "LEAN_CTX_CONFIG_DIR",
    "LEAN_CTX_STATE_DIR",
    "LEAN_CTX_CACHE_DIR",
];

/// Sets an environment variable (test-only).
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
    // SAFETY: the caller holds `test_env_lock()` for the whole test body.
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

/// A raw `ctx_read` of `path`: whether it was refused, and its text.
async fn read(engine: &ContextEngine, path: &Path) -> (bool, String) {
    let answer = engine
        .call_tool_value(
            "ctx_read",
            Some(json!({ "path": path.to_string_lossy(), "mode": "raw", "fresh": true })),
        )
        .await
        .expect("ctx_read is a tool");
    let refused = answer
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let text = answer["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    (refused, text)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_declared_root_holds_the_jail_and_is_scanned() {
    // Serializes every environment mutation in this binary.
    let _env_lock = lean_ctx::core::data_dir::test_env_lock();
    // Declared after the lock: dropping restores the environment while the lock
    // is still held.
    let _env_restore = EnvRestore::capture();

    let tmp = tempfile::tempdir().expect("temp dir for the declared-root guard");
    // The scan guard compares against `HOME` as given, so the tree is built on
    // the canonical spelling; the declaration below uses the temp dir's own
    // spelling (on macOS `/var/…` for `/private/var/…`).
    let base = std::fs::canonicalize(tmp.path()).expect("canonical temp dir");

    let home = base.join("home");
    let env = base.join("env");
    std::fs::create_dir_all(&home).expect("create the redirected home");
    set_env("HOME", &home);
    for key in MANAGED_ENV.iter().filter(|key| **key != "HOME") {
        let dir = env.join(key);
        std::fs::create_dir_all(&dir).expect("create a redirected environment dir");
        set_env(key, &dir);
    }

    let layout = base.join("layout");
    let overrides = DirOverrides {
        config: layout.join("config"),
        data: layout.join("data"),
        state: layout.join("state"),
        cache: layout.join("cache"),
    };
    for dir in [
        &overrides.config,
        &overrides.data,
        &overrides.state,
        &overrides.cache,
    ] {
        std::fs::create_dir_all(dir).expect("create an injected category dir");
    }
    assert!(
        set_dir_overrides(overrides),
        "the injected layout must be accepted"
    );
    disable_daemon_ipc();

    // The root has content and no marker, and is a direct child of the home
    // directory; its neighbour is a marked project with a file of its own.
    let notes = home.join("notes");
    std::fs::create_dir_all(&notes).expect("create the markerless root");
    std::fs::write(notes.join("note.md"), "inside the declared root\n").expect("write a note");
    let neighbour = home.join("neighbour");
    std::fs::create_dir_all(neighbour.join(".git")).expect("create the neighbour's marker");
    std::fs::write(neighbour.join("secret.txt"), "outside the declared root\n")
        .expect("write the neighbour's file");
    let secret = neighbour.join("secret.txt");

    // ---- controls: undeclared, the root is a stray working directory --------
    assert!(
        !has_project_marker(&notes),
        "control: the root has no marker"
    );
    assert!(
        !is_safe_scan_root_public(&notes.to_string_lossy()),
        "control: a markerless child of home is not scanned"
    );
    let undeclared = ContextEngine::with_project_root(&notes);
    let (refused, text) = read(&undeclared, &secret).await;
    assert!(
        !refused && text.contains("outside the declared root"),
        "control: an undeclared markerless root re-roots to the neighbour: {text}"
    );

    // ---- declare the root -----------------------------------------------------
    declare_project_root(&tmp.path().join("home").join("notes"));
    assert!(
        has_project_marker(&notes),
        "a declared root counts as marked, in its canonical spelling"
    );
    assert!(
        has_project_marker(&tmp.path().join("home").join("notes")),
        "a declared root counts as marked, as it was given"
    );
    assert!(
        !has_project_marker(&home.join("elsewhere")),
        "a declaration covers its root only"
    );
    assert!(
        is_safe_scan_root_public(&notes.to_string_lossy()),
        "a declared child of home is scanned"
    );

    // The jail holds: inside reads pass, the neighbour stays refused, and a read
    // inside still passes afterwards — the root did not move.
    let engine = ContextEngine::with_project_root(&notes);
    let (refused, text) = read(&engine, &notes.join("note.md")).await;
    assert!(
        !refused && text.contains("inside the declared root"),
        "a read inside the declared root: {text}"
    );
    for attempt in 1..=2 {
        let (refused, text) = read(&engine, &secret).await;
        assert!(
            refused,
            "attempt {attempt}: a read of the neighbour must be refused: {text}"
        );
        assert!(
            !text.contains("outside the declared root"),
            "attempt {attempt}: the refusal returned the neighbour's file: {text}"
        );
    }
    let (refused, text) = read(&engine, &notes.join("note.md")).await;
    assert!(
        !refused && text.contains("inside the declared root"),
        "a read inside after the refusals: {text}"
    );

    // Broad stays broad: the declaration does not make the home directory a
    // project.
    declare_project_root(&home);
    assert!(
        is_broad_or_unsafe_root(&home),
        "a declared home directory is still broad"
    );
    assert!(
        !is_safe_scan_root_public(&home.to_string_lossy()),
        "a declared home directory is still not scanned"
    );
}
