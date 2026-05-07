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

use std::sync::Arc;

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
            // 400 ms is tuned to the 30 fps default capture cadence: ~12
            // captured frames during the ripple, with the held-opacity
            // window covering ~7 of them. See `click_pulse` for the curve.
            click_ms: 400,
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

    /// Snapshot of the per-call knobs the click/hover hooks need, paired
    /// with the Arc-wrapped CDP client the spawned tween/ripple tasks need
    /// to keep around past the click handler's stack frame.
    pub fn bridge(&self, client: Arc<CdpClient>) -> CursorBridge {
        CursorBridge {
            tween_ms: self.tween_ms,
            click_ms: self.click_ms,
            block_clicks: self.block_clicks,
            client,
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
        let click_ms = bounded_u32(obj.get("clickMs"), "clickMs", 0, 2000, 400)?;
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

/// Cursor knobs the click/hover hooks in `interaction.rs` need, including a
/// shared `Arc<CdpClient>` so the daemon-driven tween/ripple tasks can spawn
/// onto tokio without losing access to the CDP socket.
///
/// Assembled by `actions.rs` only when a recording is active and the cursor
/// is installed (see `DaemonState::cursor_bridge`); passed as
/// `Option<&CursorBridge>` so the no-cursor call path stays zero-cost.
/// `CursorBridge` is `Clone` (the Arc is cheap to clone) but not `Copy`.
#[derive(Clone)]
pub struct CursorBridge {
    pub tween_ms: u32,
    pub click_ms: u32,
    pub block_clicks: bool,
    pub client: Arc<CdpClient>,
}

impl std::fmt::Debug for CursorBridge {
    // Manual `Debug` because `CdpClient` doesn't derive it. The bridge
    // appears in derived `Debug` for `ClickOptions` (in `interaction.rs`),
    // so we need *something*. Skip the client field to avoid pulling Debug
    // through the CDP socket layer.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CursorBridge")
            .field("tween_ms", &self.tween_ms)
            .field("click_ms", &self.click_ms)
            .field("block_clicks", &self.block_clicks)
            .field("client", &"Arc<CdpClient>")
            .finish()
    }
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

/// One step interval for daemon-driven animation. Picked to match the
/// default 30 fps capture rate so each captured frame sees a fresh cursor
/// position. Going below this floods CDP for no visible benefit; going
/// well above creates visible stutter even at 30 fps capture.
const STEP_INTERVAL: std::time::Duration = std::time::Duration::from_millis(30);

/// Read the page-side cursor's current `(x, y)`. Used by the daemon-driven
/// tween to compute the start of an interpolation. Returns `(-1000, -1000)`
/// when the cursor isn't installed yet so callers can detect the
/// first-move-after-install case and snap to the target.
async fn read_position(client: &CdpClient, session_id: &str) -> (f64, f64) {
    let expr = "window.__ab_cursor ? \
        JSON.stringify({x: window.__ab_cursor.state.x, y: window.__ab_cursor.state.y}) \
        : '{\"x\":-1000,\"y\":-1000}'";
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
    let Ok(value) = result else {
        return (-1000.0, -1000.0);
    };
    let Some(payload) = value
        .get("result")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.as_str())
    else {
        return (-1000.0, -1000.0);
    };
    let parsed: serde_json::Result<serde_json::Value> = serde_json::from_str(payload);
    match parsed {
        Ok(v) => {
            let x = v.get("x").and_then(|n| n.as_f64()).unwrap_or(-1000.0);
            let y = v.get("y").and_then(|n| n.as_f64()).unwrap_or(-1000.0);
            (x, y)
        }
        Err(_) => (-1000.0, -1000.0),
    }
}

/// Synchronously snap the cursor to `(x, y)` via a single Runtime.evaluate
/// of `__ab_cursor.setCursor`. Used for the first-move case and as the
/// step primitive in `tween_steps`.
async fn set_cursor(client: &CdpClient, session_id: &str, x: f64, y: f64) -> Result<(), String> {
    let expr = format!(
        "window.__ab_cursor && window.__ab_cursor.setCursor({}, {})",
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

async fn set_ripple(
    client: &CdpClient,
    session_id: &str,
    x: f64,
    y: f64,
    scale: f64,
    opacity: f64,
) -> Result<(), String> {
    let expr = format!(
        "window.__ab_cursor && window.__ab_cursor.setRipple({}, {}, {}, {})",
        x, y, scale, opacity
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

fn ease_out_cubic(t: f64) -> f64 {
    1.0 - (1.0 - t).powi(3)
}

/// Run the tween step loop synchronously. Each iteration computes the
/// eased intermediate position and calls `set_cursor`. Sleeps `STEP_INTERVAL`
/// between steps so the page has time to paint a fresh frame before the
/// next captureScreenshot tick.
async fn run_tween(
    client: &CdpClient,
    session_id: &str,
    from_x: f64,
    from_y: f64,
    to_x: f64,
    to_y: f64,
    tween_ms: u32,
) -> Result<(), String> {
    if tween_ms == 0 || from_x < 0.0 || from_y < 0.0 {
        return set_cursor(client, session_id, to_x, to_y).await;
    }
    let total = std::time::Duration::from_millis(tween_ms as u64);
    let steps =
        ((total.as_millis() as u64) / (STEP_INTERVAL.as_millis() as u64)).clamp(2, 120) as u32;
    for i in 1..=steps {
        let t = i as f64 / steps as f64;
        let k = ease_out_cubic(t);
        let cx = from_x + (to_x - from_x) * k;
        let cy = from_y + (to_y - from_y) * k;
        set_cursor(client, session_id, cx, cy).await?;
        if i < steps {
            tokio::time::sleep(STEP_INTERVAL).await;
        }
    }
    Ok(())
}

/// Animate the cursor toward `(x, y)` over `tween_ms`. Spawned as a
/// background task so the click pipeline can return immediately; the task
/// keeps stepping in parallel with whatever the daemon does next. Concurrent
/// tweens are not explicitly cancelled -- the most recent `setCursor`
/// always wins for the next paint, so brief overlap glitches but doesn't
/// corrupt anything.
pub async fn move_async(
    client: &Arc<CdpClient>,
    session_id: &str,
    x: f64,
    y: f64,
    tween_ms: u32,
) -> Result<(), String> {
    let (from_x, from_y) = read_position(client, session_id).await;
    if tween_ms == 0 || from_x < 0.0 || from_y < 0.0 {
        return set_cursor(client, session_id, x, y).await;
    }
    let client_arc = Arc::clone(client);
    let session = session_id.to_string();
    tokio::spawn(async move {
        let _ = run_tween(&client_arc, &session, from_x, from_y, x, y, tween_ms).await;
    });
    Ok(())
}

/// Animate the cursor toward `(x, y)` and await the final step. Used only
/// when `--cursor-block-clicks` is set: the click pipeline waits for the
/// cursor to arrive before dispatching the real CDP mouse events.
pub async fn move_blocking(
    client: &CdpClient,
    session_id: &str,
    x: f64,
    y: f64,
    tween_ms: u32,
) -> Result<(), String> {
    let (from_x, from_y) = read_position(client, session_id).await;
    run_tween(client, session_id, from_x, from_y, x, y, tween_ms).await
}

/// Animate the click ripple at `(x, y)`. Spawned as a background task so
/// the click pipeline can return immediately; the visual outlasts the
/// CDP click events by `click_ms` regardless.
///
/// Opacity is held HIGH for the first ~60% of the duration, then linearly
/// fades to 0 over the remaining 40%. This shape is deliberately tuned to
/// the recording capture cadence: at 30 fps, a `click_ms` of 400 produces
/// ~12 captured frames, ~7 of which land in the held-opacity window and
/// reliably show the ripple regardless of which exact tick aligns with
/// the click. Pure ease-out fading (the previous shape) buried the ripple
/// in invisibility for half its life and lost most of those captures.
pub async fn click_pulse(
    client: &Arc<CdpClient>,
    session_id: &str,
    x: f64,
    y: f64,
    click_ms: u32,
) -> Result<(), String> {
    let client_arc = Arc::clone(client);
    let session = session_id.to_string();
    tokio::spawn(async move {
        let click_ms = if click_ms == 0 { 400 } else { click_ms };
        let total = std::time::Duration::from_millis(click_ms as u64);
        let steps =
            ((total.as_millis() as u64) / (STEP_INTERVAL.as_millis() as u64)).clamp(4, 40) as u32;
        // Synchronous "born" frame guarantees something visible the moment
        // any captureScreenshot fires during the ripple's lifetime, even
        // if the spawned task hasn't run its first iteration yet.
        let _ = set_ripple(&client_arc, &session, x, y, 0.6, 0.85).await;
        // Hold-then-fade: opacity stays at full 0.85 until t = HOLD, then
        // linear fades to 0 by t = 1.0. Scale grows continuously across
        // the whole duration via ease-out cubic.
        const HOLD: f64 = 0.55;
        for i in 1..=steps {
            let t = i as f64 / steps as f64;
            let scale = 0.6 + (1.0 - (1.0 - t).powi(3)) * 2.0; // 0.6 -> 2.6
            let opacity = if t <= HOLD {
                0.85
            } else {
                0.85 * (1.0 - (t - HOLD) / (1.0 - HOLD))
            };
            let _ = set_ripple(&client_arc, &session, x, y, scale, opacity).await;
            if i < steps {
                tokio::time::sleep(STEP_INTERVAL).await;
            }
        }
        // Snap to invisible at the end so no ghost ring lingers on future
        // captures if the recording continues past the click.
        let _ = set_ripple(&client_arc, &session, x, y, 2.6, 0.0).await;
    });
    Ok(())
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
        assert_eq!(cfg.click_ms, 400);
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
