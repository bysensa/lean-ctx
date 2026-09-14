use anyhow::{Context, Result};
use std::sync::OnceLock;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::runtime::Runtime;

use crate::daemon;
use crate::ipc;

static DELIVERY_RUNTIME: OnceLock<Result<Runtime, String>> = OnceLock::new();

/// Cross-agent delivery lookups are an optional cache tier. They must never
/// inherit the daemon client's normal control-plane timeout on an interactive
/// read or shell path.
const BEST_EFFORT_DELIVERY_IPC_TIMEOUT: Duration = Duration::from_millis(250);

fn delivery_runtime() -> Result<&'static Runtime> {
    match DELIVERY_RUNTIME.get_or_init(|| Runtime::new().map_err(|error| error.to_string())) {
        Ok(runtime) => Ok(runtime),
        Err(error) => Err(anyhow::anyhow!("initialize delivery IPC runtime: {error}")),
    }
}

/// Safely run an async future on the delivery runtime from any thread context.
/// When called from inside a tokio runtime (e.g. tool handler async pipeline /
/// prefetch re-entry), spawns a scoped thread to avoid "Cannot start a runtime
/// from within a runtime". When called from outside (CLI), blocks directly.
fn delivery_block_on<F, O>(fut: F) -> Option<O>
where
    F: std::future::Future<Output = O> + Send,
    O: Send,
{
    let rt = delivery_runtime().ok()?;
    if tokio::runtime::Handle::try_current().is_ok() {
        let mut result = None;
        std::thread::scope(|s| {
            s.spawn(|| {
                result = Some(rt.block_on(fut));
            });
        });
        result
    } else {
        Some(rt.block_on(fut))
    }
}

/// Send an HTTP request to the daemon over the IPC channel.
/// Returns the response body as a string.
pub async fn daemon_request(method: &str, path: &str, body: &str) -> Result<String> {
    use tokio::time::timeout;

    if crate::daemon::daemon_ipc_disabled() {
        anyhow::bail!("daemon IPC disabled by embedder");
    }

    const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
    const IO_TIMEOUT: Duration = Duration::from_secs(10);

    let addr = daemon::daemon_addr();
    if !addr.is_listening() {
        anyhow::bail!(
            "Daemon endpoint not found at {}. Is the daemon running?",
            addr.display()
        );
    }

    let request = format_http_request(method, path, body);

    #[cfg(unix)]
    {
        let mut stream = timeout(CONNECT_TIMEOUT, ipc::connect(&addr))
            .await
            .with_context(|| {
                format!(
                    "connect to daemon timed out ({}s)",
                    CONNECT_TIMEOUT.as_secs()
                )
            })?
            .with_context(|| format!("cannot connect to daemon at {}", addr.display()))?;

        timeout(IO_TIMEOUT, stream.write_all(request.as_bytes()))
            .await
            .context("write to daemon timed out")?
            .context("failed to write request to daemon")?;

        let mut response_buf = Vec::with_capacity(4096);
        timeout(IO_TIMEOUT, stream.read_to_end(&mut response_buf))
            .await
            .context("read from daemon timed out")?
            .context("failed to read response from daemon")?;

        parse_http_response(&response_buf)
    }

    #[cfg(windows)]
    {
        let mut stream = timeout(CONNECT_TIMEOUT, ipc::connect(&addr))
            .await
            .with_context(|| {
                format!(
                    "connect to daemon timed out ({}s)",
                    CONNECT_TIMEOUT.as_secs()
                )
            })?
            .with_context(|| format!("cannot connect to daemon at {}", addr.display()))?;

        timeout(IO_TIMEOUT, stream.write_all(request.as_bytes()))
            .await
            .context("write to daemon timed out")?
            .context("failed to write request to daemon")?;

        let mut response_buf = Vec::with_capacity(4096);
        timeout(IO_TIMEOUT, stream.read_to_end(&mut response_buf))
            .await
            .context("read from daemon timed out")?
            .context("failed to read response from daemon")?;

        parse_http_response(&response_buf)
    }
}

/// Check if the daemon is reachable by hitting /health.
pub async fn daemon_health_check() -> bool {
    match daemon_request("GET", "/health", "").await {
        Ok(body) => body.trim() == "ok",
        Err(_) => false,
    }
}

/// Call a tool on the daemon's REST API.
pub async fn daemon_tool_call(name: &str, arguments: Option<&serde_json::Value>) -> Result<String> {
    let body = serde_json::json!({
        "name": name,
        "arguments": arguments,
    });
    daemon_request("POST", "/v1/tools/call", &body.to_string()).await
}

fn format_http_request(method: &str, path: &str, body: &str) -> String {
    if body.is_empty() {
        format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
    } else {
        let content_length = body.len();
        format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {content_length}\r\nConnection: close\r\n\r\n{body}"
        )
    }
}

fn parse_http_response(raw: &[u8]) -> Result<String> {
    let response_str = std::str::from_utf8(raw).context("daemon response is not valid UTF-8")?;

    let Some(header_end) = response_str.find("\r\n\r\n") else {
        anyhow::bail!("malformed HTTP response from daemon (no header boundary)");
    };

    let headers = &response_str[..header_end];
    let body = &response_str[header_end + 4..];

    let status_line = headers.lines().next().unwrap_or("");
    let status_code = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);

    if status_code >= 400 {
        anyhow::bail!("daemon returned HTTP {status_code}: {body}");
    }

    Ok(body.to_string())
}

/// Attempt to connect to the daemon. Returns `None` if not running.
pub async fn try_daemon_request(method: &str, path: &str, body: &str) -> Option<String> {
    if !daemon::is_daemon_running() {
        return None;
    }
    daemon_request(method, path, body).await.ok()
}

/// Tell a *running* daemon to drop its in-memory read cache (`SessionCache`).
/// Returns `true` if a daemon was reached. Never auto-starts a daemon — if none
/// is running there is no cache to flush. Force-rebuild CLI commands call this so
/// `ctx_read` map/signatures stop serving pre-rebuild output from the daemon's
/// long-lived cache, which CLI index rebuilds otherwise can't reach (#420).
pub fn notify_cache_clear() -> bool {
    if !daemon::is_daemon_running() {
        return false;
    }
    let Ok(rt) = tokio::runtime::Runtime::new() else {
        return false;
    };
    let body = serde_json::json!({
        "name": "ctx_cache",
        "arguments": { "action": "clear" },
    });
    rt.block_on(async {
        try_daemon_request("POST", "/v1/tools/call", &body.to_string())
            .await
            .is_some()
    })
}

/// Blocking helper for CLI commands: routes a tool call through the daemon.
///
/// Returns `None` if no daemon can serve the call (caller then renders the tool
/// locally / standalone). Behaviour splits on caller identity:
///
/// - **Normal CLI invocation**: connects to a running daemon, and *auto-starts*
///   one if none is listening (so the long-lived `LeanCtxServer` state — caches,
///   indexes, detectors — is reused across commands).
/// - **Shadow-mode hook child** (`LEAN_CTX_HOOK_CHILD` set): *connect-only*. It
///   reuses an already-running daemon for full parity (the `ready` fast-path
///   below routes straight through `/v1/tools/call` → `call_tool_guarded`, so the
///   in-memory `LoopDetector`, correction-loop auto-degrade, bounce tracker and
///   adaptive thresholds all fire on the daemon's long-lived state — #566), but
///   it MUST NEVER auto-start a daemon. A hook fires once per intercepted
///   read/grep as a fresh process; auto-starting from there would spawn daemons
///   uncontrollably. With no live daemon it returns `None` and the caller falls
///   back to the enriched standalone path (disk-backed learning sinks + Context
///   IR from #550/#569).
#[allow(clippy::needless_pass_by_value)]
pub fn try_daemon_tool_call_blocking(
    name: &str,
    arguments: Option<serde_json::Value>,
) -> Option<String> {
    use std::time::Duration;

    if std::env::var_os("__LEAN_CTX_NO_DAEMON").is_some() {
        return None;
    }

    let rt = Runtime::new().ok()?;

    let addr = daemon::daemon_addr();
    let mut ready = addr.is_listening() && rt.block_on(async { daemon_health_check().await });

    if !ready {
        // Connect-only for shadow-mode hooks (#566): a hook child reaches a live
        // daemon via the `ready` fast-path above (full detector parity), but when
        // none is listening it must bail to the standalone fallback instead of
        // auto-starting one. This guard MUST stay inside `if !ready` — hoisting it
        // to the top of the function would also block hooks from reusing a running
        // daemon, silently regressing loop/bounce/adaptive parity.
        if crate::core::runtime_flags::hook_child_enabled() {
            return None;
        }

        let lock = crate::core::startup_guard::try_acquire_lock(
            "daemon-start",
            Duration::from_millis(1200),
            Duration::from_secs(5),
        );

        if let Some(g) = lock {
            g.touch();
            let mut did_start = false;

            if !daemon::is_daemon_running() {
                if daemon::start_daemon(&[]).is_ok() {
                    did_start = true;
                } else {
                    return None;
                }
            }

            for _ in 0..60 {
                if addr.is_listening() && rt.block_on(async { daemon_health_check().await }) {
                    ready = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }

            if ready && did_start && crate::core::protocol::meta_visible() {
                eprintln!("\x1b[2m▸ daemon auto-started\x1b[0m");
            }
        } else {
            for _ in 0..60 {
                if addr.is_listening() && rt.block_on(async { daemon_health_check().await }) {
                    ready = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }

    if !ready {
        return None;
    }

    if let Some(out) = rt.block_on(async { daemon_tool_call(name, arguments.as_ref()).await.ok() })
    {
        return Some(out);
    }

    for _ in 0..5 {
        std::thread::sleep(Duration::from_millis(50));
        if let Some(out) =
            rt.block_on(async { daemon_tool_call(name, arguments.as_ref()).await.ok() })
        {
            return Some(out);
        }
    }

    None
}

fn unwrap_mcp_tool_text(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let result = v.get("result")?;

    if let Some(content) = result.get("content").and_then(|c| c.as_array()) {
        let mut texts: Vec<String> = Vec::new();
        for item in content {
            if let Some(text) = item.get("text").and_then(|t| t.as_str())
                && !text.is_empty()
            {
                texts.push(text.to_string());
            }
        }
        if !texts.is_empty() {
            return Some(texts.join("\n"));
        }
    }

    if let Some(text) = result.get("text").and_then(|t| t.as_str()) {
        return Some(text.to_string());
    }

    result.as_str().map(std::string::ToString::to_string)
}

/// Like `try_daemon_tool_call_blocking`, but unwraps MCP JSON responses to text for CLI output.
pub fn try_daemon_tool_call_blocking_text(
    name: &str,
    arguments: Option<serde_json::Value>,
) -> Option<String> {
    let body = try_daemon_tool_call_blocking(name, arguments)?;
    let trimmed = body.trim_start();
    if !trimmed.starts_with('{') {
        return Some(body);
    }
    Some(unwrap_mcp_tool_text(&body).unwrap_or(body))
}

/// Check the daemon's cross-agent delivery registry for a content hash.
/// Returns `None` if daemon unreachable or no hit. Connect-only — never
/// auto-starts a daemon (delivery is best-effort).
pub fn try_delivery_check_blocking(
    blake3: &[u8; 12],
    mtime: u64,
    path: &str,
    requester_agent_id: Option<&str>,
    requester_conversation_id: Option<&str>,
) -> Option<crate::core::ocla::types::DeliveryRecord> {
    if !daemon::is_daemon_running() {
        return None;
    }
    let body = serde_json::json!({
        "blake3": blake3,
        "mtime": mtime,
        "path": path,
        "requester_agent_id": requester_agent_id,
        "requester_conversation_id": requester_conversation_id,
    });
    let body_str = body.to_string();
    let resp = delivery_block_on(async move {
        tokio::time::timeout(
            BEST_EFFORT_DELIVERY_IPC_TIMEOUT,
            try_daemon_request("POST", "/ocla/v1/delivery/check", &body_str),
        )
        .await
        .ok()
        .flatten()
    })??;
    let v: serde_json::Value = serde_json::from_str(&resp).ok()?;
    if !v.get("hit")?.as_bool()? {
        return None;
    }
    Some(crate::core::ocla::types::DeliveryRecord {
        blake3: *blake3,
        path: v.get("path")?.as_str()?.to_string(),
        line_count: v.get("line_count")?.as_u64()? as u32,
        token_count: v.get("token_count").and_then(serde_json::Value::as_u64)?,
        agent_id: v.get("agent_id")?.as_str()?.to_string(),
        conversation_id: v.get("conversation_id")?.as_str()?.to_string(),
        read_at: v.get("read_at")?.as_u64()?,
        mtime: v
            .get("mtime")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(mtime),
        fresh: v
            .get("fresh")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true),
        relay_content: v
            .get("relay_content")
            .and_then(serde_json::Value::as_str)
            .map(String::from),
        relay_mode: v
            .get("relay_mode")
            .and_then(serde_json::Value::as_str)
            .map(String::from),
    })
}

/// Record a delivery in the daemon's cross-agent registry.
/// Fire-and-forget: errors and slow daemon responses are intentionally dropped.
pub fn try_delivery_record_blocking(entry: &crate::core::ocla::types::DeliveryEntry) {
    if !daemon::is_daemon_running() {
        return;
    }
    let Ok(body) = serde_json::to_string(entry) else {
        return;
    };
    let Ok(rt) = delivery_runtime() else {
        return;
    };
    drop(rt.spawn(async move {
        let _ = tokio::time::timeout(
            Duration::from_secs(3),
            try_daemon_request("POST", "/ocla/v1/delivery/record", &body),
        )
        .await;
    }));
}

/// Check the daemon's generalized cross-agent cache (all DeliveryKinds).
/// Returns the cached entry on hit, None on miss or daemon unreachable.
pub fn try_cache_check_blocking(
    key: &crate::core::ocla::cache_types::CacheKey,
    validator: &crate::core::ocla::cache_types::CacheValidator,
    requester_agent_id: Option<&str>,
    requester_conversation_id: Option<&str>,
) -> Option<crate::core::ocla::cache_types::DeliveryEntryV2> {
    if !daemon::is_daemon_running() {
        return None;
    }
    let validator_str = match validator {
        crate::core::ocla::cache_types::CacheValidator::Immutable => "immutable".into(),
        crate::core::ocla::cache_types::CacheValidator::File { mtime_ns } => {
            format!("file:{mtime_ns}")
        }
        crate::core::ocla::cache_types::CacheValidator::Directory { mtime_ns } => {
            format!("directory:{mtime_ns}")
        }
    };
    let body = serde_json::json!({
        "key": key.0,
        "validator": validator_str,
        "requester_agent_id": requester_agent_id,
        "requester_conversation_id": requester_conversation_id,
    });
    let body_str = body.to_string();
    let resp = delivery_block_on(async move {
        tokio::time::timeout(
            BEST_EFFORT_DELIVERY_IPC_TIMEOUT,
            try_daemon_request("POST", "/ocla/v1/cache/check", &body_str),
        )
        .await
        .ok()
        .flatten()
    })??;
    let v: serde_json::Value = serde_json::from_str(&resp).ok()?;
    if !v.get("hit")?.as_bool()? {
        return None;
    }
    serde_json::from_value(v.get("entry")?.clone()).ok()
}

/// Record a generalized cache entry via daemon IPC. Fire-and-forget.
pub fn try_cache_record_blocking(entry: &crate::core::ocla::cache_types::DeliveryEntryV2) {
    if !daemon::is_daemon_running() {
        return;
    }
    let Ok(body) = serde_json::to_string(entry) else {
        return;
    };
    let Ok(rt) = delivery_runtime() else { return };
    drop(rt.spawn(async move {
        let _ = tokio::time::timeout(
            Duration::from_secs(3),
            try_daemon_request("POST", "/ocla/v1/cache/record", &body),
        )
        .await;
    }));
}

#[cfg(test)]
mod tests {
    use super::delivery_runtime;

    #[test]
    fn delivery_runtime_is_shared() {
        let first = delivery_runtime().expect("shared delivery runtime initializes");
        let second = delivery_runtime().expect("shared delivery runtime remains available");
        assert!(std::ptr::eq(first, second));
    }
}
