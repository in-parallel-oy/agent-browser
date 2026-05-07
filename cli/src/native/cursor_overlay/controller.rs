//! Rust-side bridge for the in-page cursor overlay.
//!
//! Installs/removes the page-side controller via
//! `Page.addScriptToEvaluateOnNewDocument` (with `runImmediately: true` so
//! the script also evaluates against the already-loaded `about:blank` of
//! freshly-created targets), and drives the page-side `moveTo`/`click`
//! functions over `Runtime.evaluate`.
//!
//! All bridge calls are best-effort: when the cursor isn't installed (yet)
//! or the execution context has been destroyed (mid-navigation),
//! `is_transient_bridge_error` returns `true` and the call is treated as
//! a no-op `Ok(())`. Genuine connection failures (`Target closed`,
//! `not found`) propagate so the recording loop can break.

use serde_json::{json, Value};

use super::scripts::{self, MotionMode, Theme};
use crate::native::cdp::client::CdpClient;

/// User-facing configuration for the cursor overlay.
///
/// Mirrors the CLI flags exposed by `record start --cursor ...` once those
/// land in PR 2. Defaults are tuned so that "just enable it" produces a
/// clean, demo-ready cursor without further configuration.
#[derive(Debug, Clone)]
pub struct CursorOverlayConfig {
    pub theme: Theme,
    pub size_px: u32,
    pub tween_ms: u32,
    pub click_ms: u32,
    pub motion: MotionMode,
    /// When `true`, click dispatch awaits the tween before firing CDP mouse
    /// events. Default `false` (fire-and-forget tween, no added click
    /// latency).
    pub block_clicks: bool,
}

impl Default for CursorOverlayConfig {
    fn default() -> Self {
        Self {
            theme: Theme::default(),
            size_px: 28,
            tween_ms: 250,
            click_ms: 150,
            motion: MotionMode::default(),
            block_clicks: false,
        }
    }
}

impl CursorOverlayConfig {
    fn install_source(&self) -> String {
        scripts::build_install_script(
            self.theme,
            self.size_px,
            self.tween_ms,
            self.click_ms,
            self.motion,
        )
    }

    /// Snapshot of the per-call knobs the click/hover hooks need. Held by
    /// reference so the click pipeline doesn't repeatedly clone the full
    /// config.
    pub fn bridge(&self) -> CursorBridge {
        CursorBridge {
            tween_ms: self.tween_ms,
            block_clicks: self.block_clicks,
        }
    }

    /// Parse the `cursor` object the CLI parser emits on `recording_start` /
    /// `recording_restart`. Returns `Ok(None)` when the field is absent --
    /// callers treat absence as "cursor disabled." Numeric inputs are
    /// validated against the same bounds the CLI advertises so a stray API
    /// call can't blow past them.
    pub fn from_cmd_value(value: &Value) -> Result<Option<Self>, String> {
        let obj = match value.as_object() {
            Some(o) => o,
            None => return Err("'cursor' must be an object".to_string()),
        };

        let theme_str = obj.get("theme").and_then(|v| v.as_str()).unwrap_or("arrow");
        let theme = Theme::from_str_ci(theme_str)?;

        let motion_str = obj.get("motion").and_then(|v| v.as_str()).unwrap_or("auto");
        let motion = MotionMode::from_str_ci(motion_str)?;

        let size_px = bounded_u32(obj.get("size"), "size", 8, 96, 28)?;
        let tween_ms = bounded_u32(obj.get("tweenMs"), "tweenMs", 0, 2000, 250)?;
        let click_ms = bounded_u32(obj.get("clickMs"), "clickMs", 0, 2000, 150)?;
        let block_clicks = obj
            .get("blockClicks")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        Ok(Some(Self {
            theme,
            size_px,
            tween_ms,
            click_ms,
            motion,
            block_clicks,
        }))
    }
}

fn bounded_u32(
    value: Option<&Value>,
    name: &str,
    lo: u32,
    hi: u32,
    default: u32,
) -> Result<u32, String> {
    let v = match value {
        Some(v) => v,
        None => return Ok(default),
    };
    let n = v
        .as_u64()
        .ok_or_else(|| format!("cursor.{} must be a non-negative integer", name))?;
    if n > u32::MAX as u64 {
        return Err(format!("cursor.{} is out of range", name));
    }
    let n = n as u32;
    if n < lo || n > hi {
        return Err(format!(
            "cursor.{} must be between {} and {} (got {})",
            name, lo, hi, n
        ));
    }
    Ok(n)
}

/// Lightweight, copyable view of the cursor settings the click/hover hooks
/// in `interaction.rs` need. Assembled by `actions.rs` only when a recording
/// is active and the cursor is installed; passed as `Option<&CursorBridge>`
/// to keep the no-cursor call path zero-cost.
#[derive(Debug, Clone, Copy)]
pub struct CursorBridge {
    pub tween_ms: u32,
    pub block_clicks: bool,
}

/// Install the cursor overlay on the given CDP session.
///
/// Returns the script identifier produced by
/// `Page.addScriptToEvaluateOnNewDocument`, which the caller stores so it
/// can pass it to [`remove`] when recording stops.
pub async fn install(
    client: &CdpClient,
    session_id: &str,
    config: &CursorOverlayConfig,
) -> Result<String, String> {
    let source = config.install_source();

    // Register for every navigation in this session, AND run immediately on
    // any execution context that already exists. The latter handles the
    // about:blank-already-loaded race in freshly-created recording targets.
    let registered = client
        .send_command(
            "Page.addScriptToEvaluateOnNewDocument",
            Some(json!({
                "source": source,
                "runImmediately": true,
            })),
            Some(session_id),
        )
        .await?;

    let identifier = registered
        .get("identifier")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Belt-and-suspenders: also evaluate the source on the current page in
    // case `runImmediately` is unsupported by the engine. Errors here are
    // swallowed -- the new-document registration above is the load-bearing
    // path; this is a redundant, opportunistic boost.
    let _ = client
        .send_command(
            "Runtime.evaluate",
            Some(json!({
                "expression": source,
                "returnByValue": true,
            })),
            Some(session_id),
        )
        .await;

    Ok(identifier)
}

/// Remove the cursor overlay from the given CDP session.
///
/// Calls `Page.removeScriptToEvaluateOnNewDocument` so future navigations
/// don't reinstall the controller, then best-effort-evaluates
/// `window.__ab_cursor.destroy()` to tear down the host element on the
/// current page. Errors on the destroy call are intentionally swallowed.
pub async fn remove(client: &CdpClient, session_id: &str, identifier: &str) -> Result<(), String> {
    if !identifier.is_empty() {
        client
            .send_command(
                "Page.removeScriptToEvaluateOnNewDocument",
                Some(json!({ "identifier": identifier })),
                Some(session_id),
            )
            .await?;
    }

    let _ = client
        .send_command(
            "Runtime.evaluate",
            Some(json!({
                "expression": "window.__ab_cursor && window.__ab_cursor.destroy && window.__ab_cursor.destroy()",
                "returnByValue": true,
            })),
            Some(session_id),
        )
        .await;

    Ok(())
}

/// Fire the cursor tween toward `(x, y)` without awaiting completion.
///
/// Used by the default click pipeline so the recording captures motion
/// across multiple frames without adding latency to the underlying CDP
/// mouse events.
pub async fn move_async(
    client: &CdpClient,
    session_id: &str,
    x: f64,
    y: f64,
    tween_ms: u32,
) -> Result<(), String> {
    let expr = format!(
        "window.__ab_cursor && window.__ab_cursor.moveTo({}, {}, {})",
        x, y, tween_ms
    );
    let result = client
        .send_command(
            "Runtime.evaluate",
            Some(json!({
                "expression": expr,
                "returnByValue": true,
                "awaitPromise": false,
                "timeout": tween_ms + 1000,
            })),
            Some(session_id),
        )
        .await;
    classify_bridge_result(result)
}

/// Fire the cursor tween and wait for it to settle. Used only when the user
/// opts into strict visual fidelity via `--cursor-block-clicks`.
pub async fn move_blocking(
    client: &CdpClient,
    session_id: &str,
    x: f64,
    y: f64,
    tween_ms: u32,
) -> Result<(), String> {
    let expr = format!(
        "window.__ab_cursor && window.__ab_cursor.moveTo({}, {}, {})",
        x, y, tween_ms
    );
    let result = client
        .send_command(
            "Runtime.evaluate",
            Some(json!({
                "expression": expr,
                "returnByValue": true,
                "awaitPromise": true,
                "timeout": tween_ms + 1000,
            })),
            Some(session_id),
        )
        .await;
    classify_bridge_result(result)
}

/// Fire the click ripple animation at `(x, y)`. Always fire-and-forget;
/// the visual is a 150 ms transient and we never want it to gate the
/// real click.
pub async fn click_pulse(
    client: &CdpClient,
    session_id: &str,
    x: f64,
    y: f64,
) -> Result<(), String> {
    let expr = format!(
        "window.__ab_cursor && window.__ab_cursor.click({}, {})",
        x, y
    );
    let result = client
        .send_command(
            "Runtime.evaluate",
            Some(json!({
                "expression": expr,
                "returnByValue": true,
            })),
            Some(session_id),
        )
        .await;
    classify_bridge_result(result)
}

/// Map a bridge call's raw result into the public best-effort contract:
/// transient page-side conditions become `Ok(())`, genuine connection
/// failures propagate.
fn classify_bridge_result(result: Result<Value, String>) -> Result<(), String> {
    match result {
        Ok(_) => Ok(()),
        Err(e) if is_transient_bridge_error(&e) => Ok(()),
        Err(e) => Err(e),
    }
}

/// CDP errors that mean "the page-side cursor isn't available right now"
/// (mid-navigation, just-created context, page already torn down) rather
/// than "the connection is dead." Treated as no-ops by the bridge.
fn is_transient_bridge_error(message: &str) -> bool {
    const TRANSIENT_NEEDLES: &[&str] = &[
        "Promise was collected",
        "Execution context was destroyed",
        "Execution context with given id not found",
        "Cannot find context",
        "Inspected target navigated or closed",
    ];
    TRANSIENT_NEEDLES.iter().any(|n| message.contains(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_expected_values() {
        let cfg = CursorOverlayConfig::default();
        assert_eq!(cfg.theme, Theme::Arrow);
        assert_eq!(cfg.size_px, 28);
        assert_eq!(cfg.tween_ms, 250);
        assert_eq!(cfg.click_ms, 150);
        assert_eq!(cfg.motion, MotionMode::Auto);
        assert!(!cfg.block_clicks);
    }

    #[test]
    fn install_source_includes_chosen_theme_and_durations() {
        let cfg = CursorOverlayConfig {
            theme: Theme::Dot,
            size_px: 40,
            tween_ms: 400,
            click_ms: 200,
            motion: MotionMode::Always,
            block_clicks: true,
        };
        let src = cfg.install_source();
        assert!(src.contains("\"size\":40"));
        assert!(src.contains("\"tweenMs\":400"));
        assert!(src.contains("\"clickMs\":200"));
        assert!(src.contains("\"motion\":\"always\""));
        assert!(src.contains("<circle"));
    }

    #[test]
    fn transient_errors_are_swallowed() {
        let transient_messages = [
            "CDP error (Runtime.evaluate): Promise was collected",
            "Promise was collected",
            "CDP error (Runtime.evaluate): Execution context was destroyed",
            "Execution context with given id not found",
            "Cannot find context with specified id",
            "Inspected target navigated or closed",
        ];
        for msg in transient_messages {
            assert!(
                is_transient_bridge_error(msg),
                "expected transient classification for: {}",
                msg
            );
            assert!(
                classify_bridge_result(Err(msg.to_string())).is_ok(),
                "expected Ok for transient: {}",
                msg
            );
        }
    }

    #[test]
    fn target_closed_propagates() {
        let propagated = [
            "CDP error (Runtime.evaluate): Target closed",
            "not found",
            "CDP command timed out: Runtime.evaluate",
            "CDP response channel closed",
        ];
        for msg in propagated {
            assert!(
                !is_transient_bridge_error(msg),
                "expected propagation for: {}",
                msg
            );
            assert!(
                classify_bridge_result(Err(msg.to_string())).is_err(),
                "expected Err for: {}",
                msg
            );
        }
    }

    #[test]
    fn ok_results_pass_through_as_ok_unit() {
        assert!(classify_bridge_result(Ok(serde_json::json!({"value": 1}))).is_ok());
        assert!(classify_bridge_result(Ok(Value::Null)).is_ok());
    }
}
