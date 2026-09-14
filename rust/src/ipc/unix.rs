use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

fn data_dir() -> PathBuf {
    // Embedder layout (Milou, MILOU-137): the socket follows the run's state
    // category, mirroring `daemon.rs::data_dir` — see MILOU-128.
    if let Some(overrides) = crate::core::paths::dir_overrides() {
        return overrides.state.clone();
    }
    dirs::data_local_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".local/share"))
        .join("lean-ctx")
}

pub(super) fn default_socket_path() -> PathBuf {
    data_dir().join("daemon.sock")
}

pub(super) fn bind_listener(path: &Path) -> Result<tokio::net::UnixListener> {
    if path.exists() {
        std::fs::remove_file(path)
            .with_context(|| format!("remove stale socket {}", path.display()))?;
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create socket dir {}", parent.display()))?;
    }

    let listener = tokio::net::UnixListener::bind(path)
        .with_context(|| format!("bind UDS {}", path.display()))?;

    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perms)
        .with_context(|| format!("chmod 600 UDS {}", path.display()))?;

    Ok(listener)
}

pub(super) async fn connect(path: &Path) -> Result<tokio::net::UnixStream> {
    tokio::net::UnixStream::connect(path)
        .await
        .with_context(|| format!("connect to daemon at {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_socket_path_ends_with_sock() {
        let p = default_socket_path();
        assert!(p.ends_with("daemon.sock"), "got: {}", p.display());
    }

    #[tokio::test]
    async fn bind_and_connect() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("test.sock");

        let listener = bind_listener(&sock).unwrap();
        assert!(sock.exists());

        let perms = std::fs::metadata(&sock).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(perms.mode() & 0o777, 0o600);

        let connect_fut = connect(&sock);
        let accept_fut = listener.accept();
        let (conn_result, accept_result) = tokio::join!(connect_fut, accept_fut);
        assert!(conn_result.is_ok());
        assert!(accept_result.is_ok());
    }

    #[tokio::test]
    async fn bind_cleans_stale_socket() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("stale.sock");
        std::fs::write(&sock, b"stale").unwrap();

        let _listener = bind_listener(&sock).unwrap();
        assert!(sock.exists());
    }
}
