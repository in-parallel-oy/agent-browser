use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::Mutex;

use super::cdp::client::CdpClient;
use super::cdp::types::{EvaluateParams, EvaluateResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordEffectsPreset {
    Off,
    Cursor,
    Demo,
}

impl RecordEffectsPreset {
    pub fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "off" => Ok(Self::Off),
            "cursor" => Ok(Self::Cursor),
            "demo" => Ok(Self::Demo),
            other => Err(format!(
                "unknown record effects preset '{}'; valid options: cursor, demo, off",
                other
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Cursor => "cursor",
            Self::Demo => "demo",
        }
    }

    pub fn enabled(self) -> bool {
        !matches!(self, Self::Off)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordMode {
    Automation,
    Demo,
}

impl RecordMode {
    pub fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "automation" => Ok(Self::Automation),
            "demo" => Ok(Self::Demo),
            other => Err(format!(
                "unknown record mode '{}'; valid options: automation, demo",
                other
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorTheme {
    Arrow,
    Dot,
    Hand,
}

impl CursorTheme {
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "arrow" => Ok(Self::Arrow),
            "dot" => Ok(Self::Dot),
            "hand" => Ok(Self::Hand),
            other => Err(format!(
                "cursor.theme must be one of arrow, dot, hand, off (got {})",
                other
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Arrow => "arrow",
            Self::Dot => "dot",
            Self::Hand => "hand",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MotionMode {
    Auto,
    Always,
    Off,
}

impl MotionMode {
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "auto" => Ok(Self::Auto),
            "always" => Ok(Self::Always),
            "off" => Ok(Self::Off),
            other => Err(format!(
                "cursor.motion must be one of auto, always, off (got {})",
                other
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Always => "always",
            Self::Off => "off",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClickSync {
    Async,
    Block,
}

impl ClickSync {
    pub fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "async" => Ok(Self::Async),
            "block" => Ok(Self::Block),
            other => Err(format!(
                "click sync must be one of async, block (got {})",
                other
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Async => "async",
            Self::Block => "block",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    Fast,
    Animated,
}

impl InputMode {
    pub fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "fast" => Ok(Self::Fast),
            "animated" => Ok(Self::Animated),
            other => Err(format!(
                "input mode must be one of fast, animated (got {})",
                other
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayPosition {
    Top,
    Center,
    Bottom,
}

impl OverlayPosition {
    pub fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "top" => Ok(Self::Top),
            "center" => Ok(Self::Center),
            "bottom" => Ok(Self::Bottom),
            other => Err(format!(
                "overlay position must be one of top, center, bottom (got {})",
                other
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Top => "top",
            Self::Center => "center",
            Self::Bottom => "bottom",
        }
    }
}

#[derive(Debug, Clone)]
pub struct CursorEffectsConfig {
    pub theme: CursorTheme,
    pub size_px: u32,
    pub tween_ms: u32,
    pub click_ms: u32,
    pub motion: MotionMode,
    pub click_sync: ClickSync,
}

impl Default for CursorEffectsConfig {
    fn default() -> Self {
        Self {
            theme: CursorTheme::Arrow,
            size_px: 28,
            tween_ms: 250,
            click_ms: 400,
            motion: MotionMode::Auto,
            click_sync: ClickSync::Async,
        }
    }
}

impl CursorEffectsConfig {
    pub fn from_cmd_value(value: &Value) -> Result<Option<Self>, String> {
        let obj = value
            .as_object()
            .ok_or_else(|| "'cursor' must be an object or \"off\"".to_string())?;
        let theme_str = obj.get("theme").and_then(|v| v.as_str()).unwrap_or("arrow");
        if theme_str == "off" {
            return Ok(None);
        }
        let mut cfg = Self {
            theme: CursorTheme::from_str(theme_str)?,
            motion: MotionMode::from_str(
                obj.get("motion").and_then(|v| v.as_str()).unwrap_or("auto"),
            )?,
            size_px: bounded_u32(obj.get("size"), "size", 8, 96, 28)?,
            tween_ms: bounded_u32(obj.get("tweenMs"), "tweenMs", 0, 2000, 250)?,
            click_ms: bounded_u32(obj.get("clickMs"), "clickMs", 0, 2000, 400)?,
            click_sync: ClickSync::Async,
        };
        if obj
            .get("blockClicks")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            cfg.click_sync = ClickSync::Block;
        }
        if let Some(v) = obj.get("clickSync").and_then(|v| v.as_str()) {
            cfg.click_sync = ClickSync::from_str(v)?;
        }
        Ok(Some(cfg))
    }

    fn effective_tween_ms(&self) -> u32 {
        if matches!(self.motion, MotionMode::Off) {
            0
        } else {
            self.tween_ms
        }
    }
}

#[derive(Debug, Clone)]
pub struct RecordingEffectsConfig {
    pub mode: RecordMode,
    pub cursor: Option<CursorEffectsConfig>,
    pub input_mode: InputMode,
    pub input_delay_ms: u64,
}

impl RecordingEffectsConfig {
    pub fn from_cmd(
        preset: RecordEffectsPreset,
        cursor_value: Option<&Value>,
        record_mode: Option<&str>,
        click_sync: Option<&str>,
        input_mode: Option<&str>,
        input_delay_ms: Option<u64>,
    ) -> Result<Option<Self>, String> {
        if !preset.enabled() {
            return Ok(None);
        }
        let mode = match record_mode {
            Some(v) => RecordMode::from_str(v)?,
            None if matches!(preset, RecordEffectsPreset::Demo) => RecordMode::Demo,
            None => RecordMode::Automation,
        };
        let mut cursor = match cursor_value {
            Some(v) => CursorEffectsConfig::from_cmd_value(v)?,
            None => Some(CursorEffectsConfig::default()),
        };
        if let Some(ref mut cfg) = cursor {
            if matches!(mode, RecordMode::Demo) {
                let cursor_obj = cursor_value.and_then(Value::as_object);
                if cursor_obj.is_none_or(|obj| !obj.contains_key("tweenMs")) {
                    cfg.tween_ms = 700;
                }
                if cursor_obj.is_none_or(|obj| !obj.contains_key("clickMs")) {
                    cfg.click_ms = 500;
                }
            }
            if let Some(v) = click_sync {
                cfg.click_sync = ClickSync::from_str(v)?;
            } else if matches!(mode, RecordMode::Demo) {
                cfg.click_sync = ClickSync::Block;
            }
        }
        let input_mode = match input_mode {
            Some(v) => InputMode::from_str(v)?,
            None if matches!(mode, RecordMode::Demo) => InputMode::Animated,
            None => InputMode::Fast,
        };
        Ok(Some(Self {
            mode,
            cursor,
            input_mode,
            input_delay_ms: input_delay_ms.unwrap_or_else(|| {
                if matches!(input_mode, InputMode::Animated) {
                    35
                } else {
                    0
                }
            }),
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
    let Some(value) = value else {
        return Ok(default);
    };
    let n = value
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

#[derive(Clone)]
struct RecordingEffectsTarget {
    client: Arc<CdpClient>,
    session_id: String,
}

impl std::fmt::Debug for RecordingEffectsTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordingEffectsTarget")
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub struct RecordingEffectsState {
    config: RecordingEffectsConfig,
    target: Option<RecordingEffectsTarget>,
    overlay_chain_until: Option<Instant>,
    effect_active_until: Option<Instant>,
    runtime_installed: bool,
    pending_move: Option<(f64, f64)>,
    move_flush_scheduled: bool,
}

impl RecordingEffectsState {
    pub fn new(config: RecordingEffectsConfig) -> Self {
        Self {
            config,
            target: None,
            overlay_chain_until: None,
            effect_active_until: None,
            runtime_installed: false,
            pending_move: None,
            move_flush_scheduled: false,
        }
    }

    pub fn new_with_target(
        config: RecordingEffectsConfig,
        client: Arc<CdpClient>,
        session_id: String,
    ) -> Self {
        let mut state = Self::new(config);
        state.target = Some(RecordingEffectsTarget { client, session_id });
        state
    }

    pub fn config(&self) -> &RecordingEffectsConfig {
        &self.config
    }

    pub fn set_device_scale_factor(&mut self, _scale: f64) {
        // DOM-rendered effects use CSS pixels in the page. The captured screenshot path
        // already accounts for the browser device scale factor.
    }

    pub async fn install(&mut self) -> Result<(), String> {
        if self.runtime_installed {
            return Ok(());
        }
        if let Some(runtime) = self.runtime() {
            runtime.install().await?;
            self.runtime_installed = true;
        }
        Ok(())
    }

    fn runtime(&self) -> Option<RecordingEffectsRuntime> {
        self.target.as_ref().map(|target| RecordingEffectsRuntime {
            target: target.clone(),
            config: self.config.clone(),
        })
    }

    pub fn move_to(&mut self, _x: f64, _y: f64) {
        let Some(cursor) = self.config.cursor.clone() else {
            return;
        };
        let now = Instant::now();
        self.extend_effect_active_until(
            now + Duration::from_millis(cursor.effective_tween_ms() as u64),
        );
    }

    pub fn click(&mut self, x: f64, y: f64) -> Duration {
        let Some(cursor_cfg) = self.config.cursor.clone() else {
            return Duration::ZERO;
        };
        self.move_to(x, y);
        self.pending_move = None;
        let now = Instant::now();
        let tween = Duration::from_millis(cursor_cfg.effective_tween_ms() as u64);
        let click_duration = Duration::from_millis(cursor_cfg.click_ms.max(1) as u64);
        let ripple_start = now
            + if matches!(cursor_cfg.click_sync, ClickSync::Block) {
                tween
            } else {
                Duration::ZERO
            };
        self.extend_effect_active_until(ripple_start + click_duration);
        if matches!(cursor_cfg.click_sync, ClickSync::Block) {
            tween
        } else {
            Duration::ZERO
        }
    }

    pub fn key(&mut self, label: String) {
        if label.is_empty() {
            return;
        }
        let now = Instant::now();
        let duration = Duration::from_millis(900);
        self.extend_effect_active_until(now + duration);
    }

    pub fn overlay_text(&mut self, text: String, _position: OverlayPosition, duration_ms: u64) {
        if text.trim().is_empty() {
            return;
        }
        let now = Instant::now();
        let duration = Duration::from_millis(duration_ms.max(1) + 180);
        let started_at = self
            .overlay_chain_until
            .filter(|active_until| *active_until > now)
            .unwrap_or(now);
        let active_until = started_at + duration;
        self.overlay_chain_until = Some(active_until);
        self.extend_effect_active_until(active_until);
    }

    pub fn spotlight(&mut self, x: f64, y: f64, duration_ms: u64) {
        let now = Instant::now();
        let duration = Duration::from_millis(duration_ms.max(1));
        let _ = (x, y);
        self.extend_effect_active_until(now + duration);
    }

    pub fn clear_overlay(&mut self) {
        self.overlay_chain_until = None;
    }

    pub fn zoom_to(&mut self, x: f64, y: f64, scale: f64, duration_ms: Option<u64>) {
        let now = Instant::now();
        let _ = (x, y, scale);
        self.extend_effect_active_until(
            now + duration_ms
                .map(|ms| Duration::from_millis(ms.max(1)) + Duration::from_millis(700))
                .unwrap_or(Duration::from_millis(650)),
        );
    }

    pub fn zoom_reset(&mut self) {
        self.extend_effect_active_until(Instant::now() + Duration::from_millis(700));
    }

    pub fn stop_post_roll_duration(&self, now: Instant) -> Duration {
        let baseline_hold = Duration::from_millis(650);
        let Some(active_until) = self.effect_active_until else {
            return Duration::ZERO;
        };
        if let Some(remaining) = active_until.checked_duration_since(now) {
            return remaining + baseline_hold;
        }
        if now.duration_since(active_until) <= Duration::from_secs(2) {
            return baseline_hold;
        }
        Duration::ZERO
    }

    fn extend_effect_active_until(&mut self, until: Instant) {
        if self
            .effect_active_until
            .is_none_or(|current| until > current)
        {
            self.effect_active_until = Some(until);
        }
    }
}

#[derive(Clone)]
struct RecordingEffectsRuntime {
    target: RecordingEffectsTarget,
    config: RecordingEffectsConfig,
}

impl RecordingEffectsRuntime {
    async fn install(&self) -> Result<(), String> {
        self.evaluate(RECORDING_EFFECTS_RUNTIME_JS.to_string())
            .await?;
        self.configure().await
    }

    async fn cleanup(&self) -> Result<(), String> {
        self.evaluate(
            "(async () => { if (window.__agentBrowserRecordingEffects) await window.__agentBrowserRecordingEffects.cleanup(); })()"
                .to_string(),
        )
        .await
    }

    async fn configure(&self) -> Result<(), String> {
        let Some(cursor) = self.config.cursor.as_ref() else {
            return self
                .evaluate(runtime_async_call(
                    "configure({ cursor: null })".to_string(),
                ))
                .await;
        };
        let config = json!({
            "cursor": {
                "theme": cursor.theme.as_str(),
                "size": cursor.size_px,
                "tweenMs": cursor.effective_tween_ms(),
                "clickMs": cursor.click_ms,
                "motion": cursor.motion.as_str(),
                "clickSync": cursor.click_sync.as_str(),
            }
        });
        self.evaluate(runtime_async_call(format!("configure({})", config)))
            .await
    }

    async fn move_to(&self, x: f64, y: f64) -> Result<(), String> {
        self.evaluate_runtime_call(format!(
            "moveTo({}, {})",
            finite_js_number(x),
            finite_js_number(y)
        ))
        .await
    }

    async fn click(&self, x: f64, y: f64) -> Result<(), String> {
        self.evaluate_runtime_call(format!(
            "click({}, {})",
            finite_js_number(x),
            finite_js_number(y)
        ))
        .await
    }

    async fn key(&self, label: &str) -> Result<(), String> {
        self.evaluate_runtime_call(format!("key({})", js_string(label)))
            .await
    }

    async fn overlay_text(
        &self,
        text: &str,
        position: OverlayPosition,
        duration_ms: u64,
    ) -> Result<(), String> {
        self.evaluate_runtime_call(format!(
            "overlayText({}, {}, {})",
            js_string(text),
            js_string(position.as_str()),
            duration_ms.max(1)
        ))
        .await
    }

    async fn spotlight(&self, x: f64, y: f64, duration_ms: u64) -> Result<(), String> {
        self.evaluate_runtime_call(format!(
            "spotlight({}, {}, {})",
            finite_js_number(x),
            finite_js_number(y),
            duration_ms.max(1)
        ))
        .await
    }

    async fn clear_overlay(&self) -> Result<(), String> {
        self.evaluate_runtime_call("clearOverlay()".to_string())
            .await
    }

    async fn zoom_to(
        &self,
        x: f64,
        y: f64,
        scale: f64,
        duration_ms: Option<u64>,
    ) -> Result<(), String> {
        let duration = duration_ms
            .map(|ms| ms.max(1).to_string())
            .unwrap_or_else(|| "null".to_string());
        self.evaluate_runtime_call(format!(
            "zoomTo({}, {}, {}, {})",
            finite_js_number(x),
            finite_js_number(y),
            finite_js_number(scale.clamp(1.0, 3.0)),
            duration
        ))
        .await
    }

    async fn zoom_reset(&self) -> Result<(), String> {
        self.evaluate_runtime_call("zoomReset()".to_string()).await
    }

    async fn evaluate_runtime_call(&self, call: String) -> Result<(), String> {
        match self.evaluate(runtime_async_call(call.clone())).await {
            Ok(()) => Ok(()),
            Err(err) if is_missing_runtime_error(&err) => {
                self.install().await?;
                self.evaluate(runtime_async_call(call)).await
            }
            Err(err) => Err(err),
        }
    }

    async fn evaluate(&self, expression: String) -> Result<(), String> {
        let result: EvaluateResult = self
            .target
            .client
            .send_command_typed(
                "Runtime.evaluate",
                &EvaluateParams {
                    expression,
                    return_by_value: Some(true),
                    await_promise: Some(true),
                },
                Some(&self.target.session_id),
            )
            .await?;
        if let Some(details) = result.exception_details {
            let msg = details
                .exception
                .as_ref()
                .and_then(|e| e.description.as_deref())
                .unwrap_or(&details.text);
            return Err(format!("Recording effects runtime error: {}", msg));
        }
        Ok(())
    }
}

fn is_missing_runtime_error(err: &str) -> bool {
    err.contains("__agentBrowserRecordingEffects")
        || err.contains("Cannot read properties of undefined")
        || err.contains("Cannot read property")
}

#[derive(Clone, Debug)]
pub struct RecordingEffectsHandle {
    pub shared: Arc<Mutex<RecordingEffectsState>>,
}

impl RecordingEffectsHandle {
    pub async fn move_to(&self, x: f64, y: f64) {
        let should_schedule = {
            let mut guard = self.shared.lock().await;
            guard.move_to(x, y);
            guard.pending_move = Some((x, y));
            if guard.move_flush_scheduled {
                false
            } else {
                guard.move_flush_scheduled = true;
                true
            }
        };
        if should_schedule {
            let shared = self.shared.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(16)).await;
                let (runtime, point) = {
                    let mut guard = shared.lock().await;
                    guard.move_flush_scheduled = false;
                    (guard.runtime(), guard.pending_move.take())
                };
                if let (Some(runtime), Some((x, y))) = (runtime, point) {
                    let _ = runtime.move_to(x, y).await;
                }
            });
        }
    }

    pub async fn click(&self, x: f64, y: f64) -> Duration {
        let (delay, runtime) = {
            let mut guard = self.shared.lock().await;
            let delay = guard.click(x, y);
            (delay, guard.runtime())
        };
        if let Some(runtime) = runtime {
            if runtime.click(x, y).await.is_ok() {
                return Duration::ZERO;
            }
        }
        delay
    }

    pub async fn click_before_dispatch(&self, x: f64, y: f64) {
        let delay = self.click(x, y).await;
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
    }

    pub async fn key(&self, label: String) {
        let runtime = {
            let mut guard = self.shared.lock().await;
            guard.key(label.clone());
            guard.runtime()
        };
        if let Some(runtime) = runtime {
            let _ = runtime.key(&label).await;
        }
    }

    pub async fn overlay_text(
        &self,
        text: String,
        position: OverlayPosition,
        duration_ms: u64,
    ) -> Result<(), String> {
        let runtime = {
            let mut guard = self.shared.lock().await;
            guard.overlay_text(text.clone(), position, duration_ms);
            guard.runtime()
        };
        if let Some(runtime) = runtime {
            runtime.overlay_text(&text, position, duration_ms).await?;
        }
        Ok(())
    }

    pub async fn spotlight(&self, x: f64, y: f64, duration_ms: u64) -> Result<(), String> {
        let runtime = {
            let mut guard = self.shared.lock().await;
            guard.spotlight(x, y, duration_ms);
            guard.runtime()
        };
        if let Some(runtime) = runtime {
            runtime.spotlight(x, y, duration_ms).await?;
        }
        Ok(())
    }

    pub async fn clear_overlay(&self) -> Result<(), String> {
        let runtime = {
            let mut guard = self.shared.lock().await;
            guard.clear_overlay();
            guard.runtime()
        };
        if let Some(runtime) = runtime {
            runtime.clear_overlay().await?;
        }
        Ok(())
    }

    pub async fn zoom_to(
        &self,
        x: f64,
        y: f64,
        scale: f64,
        duration_ms: Option<u64>,
    ) -> Result<(), String> {
        let runtime = {
            let mut guard = self.shared.lock().await;
            guard.zoom_to(x, y, scale, duration_ms);
            guard.runtime()
        };
        if let Some(runtime) = runtime {
            runtime.zoom_to(x, y, scale, duration_ms).await?;
        }
        Ok(())
    }

    pub async fn zoom_reset(&self) -> Result<(), String> {
        let runtime = {
            let mut guard = self.shared.lock().await;
            guard.zoom_reset();
            guard.runtime()
        };
        if let Some(runtime) = runtime {
            runtime.zoom_reset().await?;
        }
        Ok(())
    }

    pub async fn cleanup(&self) -> Result<(), String> {
        let runtime = self.shared.lock().await.runtime();
        if let Some(runtime) = runtime {
            runtime.cleanup().await?;
        }
        Ok(())
    }
}

fn js_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

fn finite_js_number(value: f64) -> String {
    if value.is_finite() {
        value.to_string()
    } else {
        "0".to_string()
    }
}

fn runtime_async_call(call: String) -> String {
    format!(
        "(async () => {{ await window.__agentBrowserRecordingEffects.{}; }})()",
        call
    )
}

const RECORDING_EFFECTS_RUNTIME_JS: &str = r#"
(() => {
  const VERSION = 5;
  if (window.__agentBrowserRecordingEffects?.version === VERSION) return;

  const Z = '2147483647';
  const ns = 'http://www.w3.org/2000/svg';
  const defaultConfig = {
    cursor: {
      theme: 'arrow',
      size: 28,
      tweenMs: 250,
      clickMs: 400,
      motion: 'auto',
      clickSync: 'async',
    },
  };
  let config = structuredClone(defaultConfig);
  let root = null;
  let cursor = null;
  let cursorPath = null;
  let cursorPoint = null;
  let cursorAnimation = null;
  let cursorVisible = false;
  let overlayChain = Promise.resolve();
  let overlayGeneration = 0;
  let zoomResetTimer = null;
  let zoomOriginalStyles = null;
  let installedStyle = null;

  function ensureRoot() {
    if (root && root.isConnected) return root;
    installedStyle = document.createElement('style');
    installedStyle.setAttribute('data-agent-browser-recording-style', '');
    installedStyle.textContent = `
      [data-agent-browser-recording-root] {
        position: fixed;
        inset: 0;
        width: 100vw;
        height: 100vh;
        overflow: hidden;
        pointer-events: none;
        z-index: ${Z};
        contain: layout style paint;
      }
      [data-agent-browser-recording-root] * {
        box-sizing: border-box;
      }
    `;
    document.documentElement.appendChild(installedStyle);
    root = document.createElement('div');
    root.setAttribute('data-agent-browser-recording-root', '');
    document.documentElement.appendChild(root);
    return root;
  }

  function cursorShape(theme) {
    if (theme === 'dot') {
      return {
        viewBox: '0 0 24 24',
        path: '<circle cx="12" cy="12" r="7.5"></circle>',
        tipX: 12 / 24,
        tipY: 12 / 24,
      };
    }
    if (theme === 'hand') {
      return {
        viewBox: '0 0 24 24',
        path: '<path d="M9.5 1C10.88 1 12 2.12 12 3.5v3.3a3.05 3.05 0 0 1 2.1-.1l4.5 1.3A3.2 3.2 0 0 1 21 11.1v4.4A7.5 7.5 0 0 1 13.5 23c-4.9 0-7.2-2.8-9.2-7.2-.9-2.1-1.5-3.5-1.8-4.3-.4-1.1.3-2.5 1.7-2.6 1.1-.1 2 .2 2.8.8V3.5C7 2.12 8.12 1 9.5 1Zm0 2A.5.5 0 0 0 9 3.5V14H7.2l-.2-.8c-.3-1.5-1.1-2.3-2-2.4.3.8.8 2 1.6 3.8 1.8 4 3.3 6.4 6.9 6.4a5.5 5.5 0 0 0 5.5-5.5v-4.4c0-.5-.3-1-.8-1.1l-4.6-1.3A1.25 1.25 0 0 0 12 9.9V12h-2V3.5A.5.5 0 0 0 9.5 3Z"></path>',
        tipX: 9.5 / 24,
        tipY: 1 / 24,
      };
    }
    return {
      viewBox: '0 0 24 24',
      path: '<path d="M15.38 13.5 17.94 20.5 13.24 22.22 10.69 15.2 6.79 17.65 8.41 1.63 19.95 12.86 15.38 13.5Zm0 5.82-2.72-7.46 2.96-.41-5.64-5.49-.79 7.83 2.53-1.59 2.72 7.46.94-.34Z"></path>',
      tipX: 8.4 / 24,
      tipY: 1.6 / 24,
    };
  }

  function ensureCursor() {
    if (!config.cursor || config.cursor.motion === 'off') return null;
    ensureRoot();
    if (cursor && cursor.isConnected) return cursor;
    cursor = document.createElementNS(ns, 'svg');
    cursor.setAttribute('data-agent-browser-recording-cursor', '');
    cursor.style.position = 'absolute';
    cursor.style.left = '0';
    cursor.style.top = '0';
    cursor.style.opacity = '0';
    cursor.style.fill = '#fff';
    cursor.style.stroke = 'rgba(0,0,0,.82)';
    cursor.style.strokeWidth = '1.25';
    cursor.style.strokeLinejoin = 'round';
    cursor.style.paintOrder = 'stroke fill';
    cursor.style.filter = 'drop-shadow(0 2px 2px rgba(0,0,0,.55))';
    cursor.style.zIndex = '60';
    cursor.style.pointerEvents = 'none';
    cursor.style.willChange = 'transform, opacity';
    cursor.style.transformOrigin = '0 0';
    cursorPath = document.createElementNS(ns, 'g');
    cursor.appendChild(cursorPath);
    root.appendChild(cursor);
    updateCursorShape();
    return cursor;
  }

  function updateCursorShape() {
    if (!cursor || !config.cursor) return;
    const size = Math.max(8, Math.min(96, Number(config.cursor.size) || 28));
    const shape = cursorShape(config.cursor.theme);
    cursor.setAttribute('viewBox', shape.viewBox);
    cursor.setAttribute('width', String(size));
    cursor.setAttribute('height', String(size));
    cursor.dataset.tipX = String(shape.tipX * size);
    cursor.dataset.tipY = String(shape.tipY * size);
    cursorPath.innerHTML = shape.path;
  }

  function cursorTransform(point) {
    const size = Math.max(8, Math.min(96, Number(config.cursor?.size) || 28));
    const tipX = Number(cursor?.dataset.tipX || size * 0.35);
    const tipY = Number(cursor?.dataset.tipY || size * 0.08);
    return `translate3d(${point.x - tipX}px, ${point.y - tipY}px, 0)`;
  }

  function visualCursorPoint() {
    if (!cursor) return null;
    const transform = getComputedStyle(cursor).transform;
    if (!transform || transform === 'none') return cursorPoint;
    try {
      const matrix = new DOMMatrixReadOnly(transform);
      const size = Math.max(8, Math.min(96, Number(config.cursor?.size) || 28));
      const tipX = Number(cursor.dataset.tipX || size * 0.35);
      const tipY = Number(cursor.dataset.tipY || size * 0.08);
      return { x: matrix.m41 + tipX, y: matrix.m42 + tipY };
    } catch (_) {
      return cursorPoint;
    }
  }

  function initialCursorPoint(to) {
    const size = Math.max(8, Math.min(96, Number(config.cursor?.size) || 28));
    const vw = window.innerWidth || document.documentElement.clientWidth || 1;
    const vh = window.innerHeight || document.documentElement.clientHeight || 1;
    const margin = size * 1.4;
    const x = to.x < vw / 2 ? -margin : vw + margin;
    const y = Math.max(margin, Math.min(vh - margin, to.y + 72));
    return { x, y };
  }

  async function moveTo(x, y, options = {}) {
    if (!config.cursor || config.cursor.motion === 'off') return;
    const el = ensureCursor();
    if (!el) return;
    updateCursorShape();
    const to = { x: Number(x) || 0, y: Number(y) || 0 };
    const from = visualCursorPoint() || initialCursorPoint(to);
    const duration = Math.max(0, Number(options.durationMs ?? config.cursor.tweenMs) || 0);
    cursorVisible = true;
    cursorAnimation?.cancel();
    if (duration === 0) {
      el.style.opacity = '1';
      el.style.transform = cursorTransform(to);
      cursorPoint = to;
      return;
    }
    el.style.opacity = '1';
    el.style.transform = cursorTransform(from);
    cursorAnimation = el.animate(
      [
        { transform: cursorTransform(from), opacity: 1 },
        { transform: cursorTransform(to), opacity: 1 },
      ],
      { duration, easing: 'cubic-bezier(0.45, 0, 0.15, 1)', fill: 'forwards' }
    );
    await Promise.all([
      cursorAnimation.finished.catch(() => {}),
      new Promise(resolve => setTimeout(resolve, duration)),
    ]);
    el.style.opacity = '1';
    el.style.transform = cursorTransform(to);
    cursorPoint = to;
  }

  function burst(x, y, duration) {
    ensureRoot();
    const ms = Math.max(260, Number(duration) || 500);
    const group = document.createElement('div');
    group.setAttribute('data-agent-browser-recording-click', '');
    group.style.position = 'absolute';
    group.style.left = `${x}px`;
    group.style.top = `${y}px`;
    group.style.width = '0';
    group.style.height = '0';
    group.style.pointerEvents = 'none';
    group.style.zIndex = '50';
    root.appendChild(group);

    const rings = [];
    for (let i = 0; i < 2; i += 1) {
      const ring = document.createElement('div');
      ring.style.position = 'absolute';
      ring.style.left = '-10px';
      ring.style.top = '-10px';
      ring.style.width = '20px';
      ring.style.height = '20px';
      ring.style.border = i === 0 ? '3px solid rgba(255,255,255,.96)' : '2px solid rgba(118,255,56,.92)';
      ring.style.borderRadius = '999px';
      ring.style.background = 'transparent';
      ring.style.boxShadow = i === 0
        ? '0 0 0 1px rgba(0,0,0,.55), 0 0 18px rgba(255,255,255,.35)'
        : '0 0 0 1px rgba(0,0,0,.45), 0 0 24px rgba(118,255,56,.55)';
      ring.style.transformOrigin = 'center center';
      group.appendChild(ring);
      rings.push(ring);
    }

    const rays = [];
    for (let i = 0; i < 8; i += 1) {
      const ray = document.createElement('div');
      ray.style.position = 'absolute';
      ray.style.width = '16px';
      ray.style.height = '3px';
      ray.style.borderRadius = '2px';
      ray.style.background = i % 2 === 0 ? '#fff' : '#76ff38';
      ray.style.boxShadow = '0 0 0 1px rgba(0,0,0,.55), 0 0 10px rgba(118,255,56,.60)';
      ray.style.transformOrigin = 'center center';
      group.appendChild(ray);
      rays.push(ray);
    }
    rings.forEach((ring, i) => ring.animate(
      [
        { transform: 'translate(-50%, -50%) scale(.35)', opacity: i === 0 ? .95 : .75 },
        { transform: `translate(-50%, -50%) scale(${i === 0 ? 3.2 : 4.6})`, opacity: 0 },
      ],
      { duration: ms + i * 120, easing: 'cubic-bezier(0, 0, 0.2, 1)', fill: 'forwards' }
    ));
    rays.forEach((ray, i) => {
      const a = (i / rays.length) * Math.PI * 2;
      const dx = Math.cos(a);
      const dy = Math.sin(a);
      const deg = a * 180 / Math.PI;
      ray.animate(
        [
          { transform: `translate(${dx * 12 - 8}px, ${dy * 12 - 1.5}px) rotate(${deg}deg) scaleX(.15)`, opacity: 0 },
          { transform: `translate(${dx * 24 - 8}px, ${dy * 24 - 1.5}px) rotate(${deg}deg) scaleX(1)`, opacity: 1, offset: .32 },
          { transform: `translate(${dx * 44 - 8}px, ${dy * 44 - 1.5}px) rotate(${deg}deg) scaleX(.2)`, opacity: 0 },
        ],
        { duration: ms, delay: 35 + i * 18, easing: 'cubic-bezier(0, 0, 0.2, 1)', fill: 'forwards' }
      );
    });
    setTimeout(() => group.remove(), ms + 360);
  }

  async function click(x, y) {
    const point = { x: Number(x) || 0, y: Number(y) || 0 };
    const clickMs = Math.max(260, Number(config.cursor?.clickMs) || 500);
    if (config.cursor?.clickSync === 'block') {
      await moveTo(point.x, point.y);
    } else {
      moveTo(point.x, point.y);
    }
    burst(point.x, point.y, clickMs);
  }

  function pill(text, position, opacity = 1) {
    ensureRoot();
    const wrap = document.createElement('div');
    wrap.style.position = 'absolute';
    wrap.style.left = '0';
    wrap.style.width = '100%';
    wrap.style.display = 'flex';
    wrap.style.justifyContent = 'center';
    wrap.style.padding = '0 24px';
    wrap.style.opacity = String(opacity);
    wrap.style.transition = 'opacity 180ms ease';
    wrap.style.zIndex = '40';
    if (position === 'top') wrap.style.top = '28px';
    else if (position === 'center') {
      wrap.style.top = '50%';
      wrap.style.transform = 'translateY(-50%)';
    } else wrap.style.bottom = '72px';
    const inner = document.createElement('div');
    inner.textContent = String(text || '');
    inner.style.maxWidth = '80%';
    inner.style.padding = '8px 20px';
    inner.style.borderRadius = '8px';
    inner.style.background = 'rgba(0,0,0,.75)';
    inner.style.color = '#fff';
    inner.style.font = '18px/1.4 system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif';
    inner.style.textAlign = 'center';
    inner.style.backdropFilter = 'blur(4px)';
    inner.style.boxShadow = '0 4px 18px rgba(0,0,0,.18)';
    wrap.appendChild(inner);
    root.appendChild(wrap);
    return wrap;
  }

  function overlayText(text, position = 'bottom', durationMs = 5000) {
    const previous = overlayChain;
    const generation = overlayGeneration;
    const shown = previous.then(() => {
      if (generation !== overlayGeneration) return null;
      root?.querySelectorAll('[data-agent-browser-recording-overlay]').forEach(el => el.remove());
      const el = pill(text, position, 0);
      el.setAttribute('data-agent-browser-recording-overlay', '');
      void el.offsetWidth;
      el.style.opacity = '1';
      return el;
    });
    overlayChain = shown.then(() => new Promise(resolve => {
      setTimeout(resolve, Math.max(1, Number(durationMs) || 5000));
    })).catch(() => {});
    return shown.then(() => undefined);
  }

  function key(label) {
    ensureRoot();
    const el = document.createElement('div');
    el.setAttribute('data-agent-browser-recording-key', '');
    el.textContent = String(label || '');
    Object.assign(el.style, {
      position: 'absolute',
      top: '18px',
      left: '50%',
      transform: 'translateX(-50%)',
      maxWidth: '72%',
      padding: '5px 10px',
      borderRadius: '7px',
      background: 'rgba(17,24,39,.82)',
      color: '#fff',
      font: '13px/1.35 ui-monospace, SFMono-Regular, Menlo, Consolas, monospace',
      boxShadow: '0 3px 12px rgba(0,0,0,.18)',
      whiteSpace: 'nowrap',
      overflow: 'hidden',
      textOverflow: 'ellipsis',
      opacity: '0',
      transition: 'opacity 160ms ease',
      zIndex: '45',
    });
    root.appendChild(el);
    requestAnimationFrame(() => { el.style.opacity = '1'; });
    setTimeout(() => {
      el.style.opacity = '0';
      setTimeout(() => el.remove(), 180);
    }, 850);
  }

  function spotlight(x, y, durationMs = 1200) {
    ensureRoot();
    root.querySelectorAll('[data-agent-browser-recording-spotlight]').forEach(el => el.remove());
    const el = document.createElement('div');
    el.setAttribute('data-agent-browser-recording-spotlight', '');
    const px = Number(x) || 0;
    const py = Number(y) || 0;
    Object.assign(el.style, {
      position: 'absolute',
      inset: '0',
      background: `radial-gradient(circle at ${px}px ${py}px, rgba(118,255,56,.18) 0, rgba(118,255,56,.10) 64px, rgba(0,0,0,.18) 128px, rgba(0,0,0,.52) 240px)`,
      opacity: '0',
      transition: 'opacity 180ms ease',
      zIndex: '20',
    });
    const ring = document.createElement('div');
    Object.assign(ring.style, {
      position: 'absolute',
      left: `${px - 72}px`,
      top: `${py - 72}px`,
      width: '144px',
      height: '144px',
      border: '3px solid rgba(118,255,56,.98)',
      borderRadius: '999px',
      boxShadow: '0 0 0 2px rgba(0,0,0,.55), 0 0 36px rgba(118,255,56,.72), inset 0 0 24px rgba(118,255,56,.28)',
    });
    el.appendChild(ring);
    root.appendChild(el);
    requestAnimationFrame(() => { el.style.opacity = '1'; });
    setTimeout(() => {
      if (!el.isConnected) return;
      el.style.opacity = '0';
      setTimeout(() => el.remove(), 180);
    }, Math.max(1, Number(durationMs) || 1200));
  }

  function clearOverlay() {
    ensureRoot();
    overlayGeneration += 1;
    root.querySelectorAll('[data-agent-browser-recording-overlay], [data-agent-browser-recording-key], [data-agent-browser-recording-spotlight]').forEach(el => el.remove());
    overlayChain = Promise.resolve();
  }

  function snapshotZoomStyles() {
    if (zoomOriginalStyles) return;
    const body = document.body;
    if (!body) return;
    zoomOriginalStyles = {
      htmlOverflow: document.documentElement.style.overflow,
      bodyOverflow: body.style.overflow,
      bodyTransformOrigin: body.style.transformOrigin,
      bodyTransition: body.style.transition,
      bodyTransform: body.style.transform,
    };
  }

  function restoreZoomStyles() {
    const body = document.body;
    if (!body || !zoomOriginalStyles) return;
    document.documentElement.style.overflow = zoomOriginalStyles.htmlOverflow;
    body.style.overflow = zoomOriginalStyles.bodyOverflow;
    body.style.transformOrigin = zoomOriginalStyles.bodyTransformOrigin;
    body.style.transition = zoomOriginalStyles.bodyTransition;
    body.style.transform = zoomOriginalStyles.bodyTransform;
    zoomOriginalStyles = null;
  }

  function zoomTo(x, y, scale, durationMs = null) {
    const s = Math.max(1, Math.min(3, Number(scale) || 1));
    clearTimeout(zoomResetTimer);
    const body = document.body;
    if (!body) return;
    snapshotZoomStyles();
    const vw = window.innerWidth || document.documentElement.clientWidth || 1;
    const vh = window.innerHeight || document.documentElement.clientHeight || 1;
    const cx = Number(x) || 0;
    const cy = Number(y) || vh;
    let originX = 0;
    let originY = vh;
    if (s !== 1) {
      originX = (s * cx - vw / 2) / (s - 1);
      originY = (s * cy - vh / 2) / (s - 1);
      originX = Math.max(0, Math.min(vw, originX));
      originY = Math.max(0, Math.min(vh, originY));
    }
    document.documentElement.style.overflow = 'hidden';
    body.style.overflow = 'hidden';
    body.style.transformOrigin = `${originX}px ${originY}px`;
    body.style.transition = 'transform 600ms cubic-bezier(0.4, 0, 0.2, 1)';
    body.style.transform = `scale(${s})`;
    if (durationMs !== null && durationMs !== undefined) {
      zoomResetTimer = setTimeout(() => zoomReset(), Math.max(1, Number(durationMs) || 1));
    }
  }

  async function zoomReset() {
    clearTimeout(zoomResetTimer);
    const body = document.body;
    if (!body) return;
    body.style.transition = 'transform 600ms cubic-bezier(0.4, 0, 0.2, 1)';
    body.style.transform = '';
    await new Promise(resolve => setTimeout(resolve, 700));
    restoreZoomStyles();
  }

  function cleanup() {
    clearTimeout(zoomResetTimer);
    root?.remove();
    installedStyle?.remove();
    root = null;
    cursor = null;
    cursorPath = null;
    cursorPoint = null;
    cursorAnimation = null;
    cursorVisible = false;
    overlayGeneration += 1;
    overlayChain = Promise.resolve();
    restoreZoomStyles();
  }

  window.__agentBrowserRecordingEffects = {
    version: VERSION,
    configure(nextConfig = {}) {
      config = { ...defaultConfig, ...nextConfig };
      if (nextConfig.cursor === null) config.cursor = null;
      else config.cursor = { ...defaultConfig.cursor, ...(nextConfig.cursor || {}) };
      updateCursorShape();
    },
    moveTo,
    click,
    key,
    overlayText,
    spotlight,
    clearOverlay,
    zoomTo,
    zoomReset,
    cleanup,
  };
})();
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_effect_presets() {
        assert_eq!(
            RecordEffectsPreset::from_str("off").unwrap(),
            RecordEffectsPreset::Off
        );
        assert_eq!(
            RecordEffectsPreset::from_str("cursor").unwrap(),
            RecordEffectsPreset::Cursor
        );
        assert_eq!(
            RecordEffectsPreset::from_str("demo").unwrap(),
            RecordEffectsPreset::Demo
        );
        assert!(RecordEffectsPreset::from_str("other").is_err());
    }

    #[test]
    fn demo_mode_defaults_to_block_clicks_and_animated_input() {
        let cfg = RecordingEffectsConfig::from_cmd(
            RecordEffectsPreset::Demo,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap()
        .unwrap();

        assert_eq!(cfg.mode, RecordMode::Demo);
        assert_eq!(cfg.input_mode, InputMode::Animated);
        let cursor = cfg.cursor.unwrap();
        assert_eq!(cursor.click_sync, ClickSync::Block);
        assert_eq!(cursor.tween_ms, 700);
        assert_eq!(cursor.click_ms, 500);
    }

    #[test]
    fn demo_mode_preserves_explicit_cursor_timing() {
        let cfg = RecordingEffectsConfig::from_cmd(
            RecordEffectsPreset::Demo,
            Some(&json!({ "theme": "arrow", "tweenMs": 320, "clickMs": 220 })),
            None,
            None,
            None,
            None,
        )
        .unwrap()
        .unwrap();

        let cursor = cfg.cursor.unwrap();
        assert_eq!(cursor.tween_ms, 320);
        assert_eq!(cursor.click_ms, 220);
    }

    #[test]
    fn demo_mode_theme_only_cursor_keeps_demo_timing() {
        let cfg = RecordingEffectsConfig::from_cmd(
            RecordEffectsPreset::Demo,
            Some(&json!({ "theme": "arrow" })),
            None,
            None,
            None,
            None,
        )
        .unwrap()
        .unwrap();

        let cursor = cfg.cursor.unwrap();
        assert_eq!(cursor.theme, CursorTheme::Arrow);
        assert_eq!(cursor.tween_ms, 700);
        assert_eq!(cursor.click_ms, 500);
    }

    #[test]
    fn click_extends_post_roll_without_auto_zoom_state() {
        let cfg = RecordingEffectsConfig::from_cmd(
            RecordEffectsPreset::Demo,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap()
        .unwrap();
        let mut state = RecordingEffectsState::new(cfg);
        let delay = state.click(120.0, 80.0);

        assert_eq!(delay, Duration::from_millis(700));
        assert!(state
            .stop_post_roll_duration(Instant::now())
            .ge(&Duration::from_millis(1_100)));
    }

    #[test]
    fn stop_post_roll_waits_for_effect_tail() {
        let cfg = RecordingEffectsConfig::from_cmd(
            RecordEffectsPreset::Cursor,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap()
        .unwrap();
        let mut state = RecordingEffectsState::new(cfg);
        let now = Instant::now();
        state.zoom_to(120.0, 80.0, 1.5, Some(1_200));

        assert!(state.stop_post_roll_duration(now) >= Duration::from_millis(1_700));
    }

    #[test]
    fn zoom_without_duration_keeps_transition_tail_until_reset() {
        let cfg = RecordingEffectsConfig::from_cmd(
            RecordEffectsPreset::Cursor,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap()
        .unwrap();
        let mut state = RecordingEffectsState::new(cfg);
        state.zoom_to(120.0, 80.0, 1.6, None);

        assert!(state.stop_post_roll_duration(Instant::now()) >= Duration::from_millis(650));
        state.zoom_reset();
        assert!(state.stop_post_roll_duration(Instant::now()) >= Duration::from_millis(1_300));
    }

    #[test]
    fn overlay_timing_serializes_for_post_roll() {
        let cfg = RecordingEffectsConfig::from_cmd(
            RecordEffectsPreset::Cursor,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap()
        .unwrap();
        let mut state = RecordingEffectsState::new(cfg);
        state.overlay_text("first".to_string(), OverlayPosition::Bottom, 1_000);
        let first_until = state.overlay_chain_until.unwrap();
        state.overlay_text("second".to_string(), OverlayPosition::Bottom, 1_000);

        assert!(state.overlay_chain_until.unwrap() >= first_until + Duration::from_millis(1_180));
    }

    #[test]
    fn runtime_script_defines_dom_only_effects() {
        assert!(RECORDING_EFFECTS_RUNTIME_JS.contains("__agentBrowserRecordingEffects"));
        assert!(RECORDING_EFFECTS_RUNTIME_JS.contains("data-agent-browser-recording-root"));
        assert!(RECORDING_EFFECTS_RUNTIME_JS.contains("initialCursorPoint"));
        assert!(RECORDING_EFFECTS_RUNTIME_JS.contains("visualCursorPoint"));
        assert!(RECORDING_EFFECTS_RUNTIME_JS.contains("overlayChain"));
        assert!(RECORDING_EFFECTS_RUNTIME_JS.contains("data-agent-browser-recording-click"));
        assert!(RECORDING_EFFECTS_RUNTIME_JS.contains("data-agent-browser-recording-spotlight"));
        assert!(!RECORDING_EFFECTS_RUNTIME_JS.contains("composite_frame"));
    }
}
