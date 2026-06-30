use std::collections::VecDeque;
use std::io::Cursor;
use std::sync::Arc;
use std::time::{Duration, Instant};

use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView, Rgba, RgbaImage};
use serde_json::Value;
use tokio::sync::Mutex;

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

#[derive(Debug)]
pub struct RecordingEffectsState {
    config: RecordingEffectsConfig,
    device_scale_factor: f64,
    cursor: Option<Point>,
    cursor_from: Option<Point>,
    cursor_to: Option<Point>,
    cursor_started_at: Instant,
    cursor_duration: Duration,
    ripple: Option<TimedPoint>,
    spotlight: Option<TimedPoint>,
    key_badge: Option<TimedLabel>,
    overlay: Option<TimedOverlay>,
    overlay_queue: VecDeque<TimedOverlay>,
    zoom: Option<TimedZoom>,
    effect_active_until: Option<Instant>,
}

impl RecordingEffectsState {
    pub fn new(config: RecordingEffectsConfig) -> Self {
        Self {
            config,
            device_scale_factor: 1.0,
            cursor: None,
            cursor_from: None,
            cursor_to: None,
            cursor_started_at: Instant::now(),
            cursor_duration: Duration::ZERO,
            ripple: None,
            spotlight: None,
            key_badge: None,
            overlay: None,
            overlay_queue: VecDeque::new(),
            zoom: None,
            effect_active_until: None,
        }
    }

    pub fn config(&self) -> &RecordingEffectsConfig {
        &self.config
    }

    pub fn set_device_scale_factor(&mut self, scale: f64) {
        self.device_scale_factor = scale.clamp(0.1, 8.0);
    }

    pub fn move_to(&mut self, x: f64, y: f64) {
        let Some(cursor) = self.config.cursor.clone() else {
            return;
        };
        let to = Point { x, y };
        let now = Instant::now();
        self.cursor_from = self.current_cursor(now).or(Some(to));
        self.cursor_to = Some(to);
        self.cursor_started_at = now;
        self.cursor_duration = Duration::from_millis(cursor.effective_tween_ms() as u64);
        if self.cursor_duration.is_zero() {
            self.cursor = Some(to);
        }
        self.extend_effect_active_until(now + self.cursor_duration);
    }

    pub fn click(&mut self, x: f64, y: f64) -> Duration {
        let Some(cursor_cfg) = self.config.cursor.clone() else {
            return Duration::ZERO;
        };
        self.move_to(x, y);
        let now = Instant::now();
        let point = Point { x, y };
        let tween = Duration::from_millis(cursor_cfg.effective_tween_ms() as u64);
        let ripple_start = if matches!(cursor_cfg.click_sync, ClickSync::Block) {
            now + tween
        } else {
            now
        };
        let click_duration = Duration::from_millis(cursor_cfg.click_ms.max(1) as u64);
        self.ripple = Some(TimedPoint::new(point, ripple_start, click_duration));
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
        self.key_badge = Some(TimedLabel {
            label: truncate_label(&label, 24),
            started_at: now,
            duration,
        });
        self.extend_effect_active_until(now + duration);
    }

    pub fn overlay_text(&mut self, text: String, position: OverlayPosition, duration_ms: u64) {
        if text.trim().is_empty() {
            return;
        }
        let now = Instant::now();
        let duration = Duration::from_millis(duration_ms.max(1));
        self.advance_overlay(now);
        let started_at = self
            .overlay_queue
            .back()
            .map(TimedOverlay::active_until)
            .or_else(|| self.overlay.as_ref().map(TimedOverlay::active_until))
            .filter(|active_until| *active_until > now)
            .unwrap_or(now);
        let overlay = TimedOverlay {
            text: truncate_label(text.trim(), 96),
            position,
            started_at,
            duration,
        };
        if self.overlay.is_some() || !self.overlay_queue.is_empty() {
            self.overlay_queue.push_back(overlay);
        } else {
            self.overlay = Some(overlay);
        }
        self.extend_effect_active_until(started_at + duration);
    }

    pub fn spotlight(&mut self, x: f64, y: f64, duration_ms: u64) {
        let now = Instant::now();
        let duration = Duration::from_millis(duration_ms.max(1));
        self.spotlight = Some(TimedPoint::new(Point { x, y }, now, duration));
        self.extend_effect_active_until(now + duration);
    }

    pub fn clear_overlay(&mut self) {
        self.overlay = None;
        self.overlay_queue.clear();
        self.spotlight = None;
        self.key_badge = None;
    }

    pub fn zoom_to(&mut self, x: f64, y: f64, scale: f64, duration_ms: Option<u64>) {
        let now = Instant::now();
        let duration = duration_ms.map(|ms| Duration::from_millis(ms.max(1)));
        self.zoom = Some(TimedZoom {
            point: Point { x, y },
            scale: scale.clamp(1.0, 3.0),
            started_at: now,
            duration,
        });
        self.extend_effect_active_until(now + duration.unwrap_or(Duration::from_millis(650)));
    }

    pub fn zoom_reset(&mut self) {
        self.zoom = None;
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

    pub async fn process_frame(shared: &Arc<Mutex<Self>>, bytes: Vec<u8>) -> Vec<u8> {
        let plan = {
            let mut guard = shared.lock().await;
            guard.frame_plan(Instant::now())
        };
        composite_frame(bytes, plan).unwrap_or_else(|fallback| fallback)
    }

    fn frame_plan(&mut self, now: Instant) -> FrameEffects {
        self.advance_overlay(now);

        let cursor_cfg = self.config.cursor.clone();
        let cursor = cursor_cfg.and_then(|cfg| self.current_cursor(now).map(|p| (p, cfg)));

        let ripple = self
            .ripple
            .and_then(|r| timed_point_state(r, now, 0.55, 2.6));
        if self.ripple.is_some_and(|r| r.expired(now)) {
            self.ripple = None;
        }

        let spotlight = self
            .spotlight
            .and_then(|s| timed_point_state(s, now, 0.70, 1.15));
        if self.spotlight.is_some_and(|s| s.expired(now)) {
            self.spotlight = None;
        }

        let key_badge = self.key_badge.as_ref().and_then(|k| {
            let t = progress(k.started_at, k.duration, now)?;
            Some(LabelFrame {
                text: k.label.clone(),
                opacity: fade_hold(t, 0.65),
                position: OverlayPosition::Top,
            })
        });
        if self
            .key_badge
            .as_ref()
            .is_some_and(|k| expired_at(k.started_at, k.duration, now))
        {
            self.key_badge = None;
        }

        let overlay = self.overlay.as_ref().and_then(|o| {
            let t = progress(o.started_at, o.duration, now)?;
            Some(LabelFrame {
                text: o.text.clone(),
                opacity: fade_hold(t, 0.75),
                position: o.position,
            })
        });
        let zoom = self.zoom.as_ref().and_then(|z| camera_state(z, now));
        if self
            .zoom
            .as_ref()
            .is_some_and(|z| z.duration.is_some_and(|d| expired_at(z.started_at, d, now)))
        {
            self.zoom = None;
        }

        FrameEffects {
            pixel_scale: self.device_scale_factor,
            cursor,
            ripple,
            spotlight,
            key_badge,
            overlay,
            zoom,
        }
    }

    fn advance_overlay(&mut self, now: Instant) {
        if self
            .overlay
            .as_ref()
            .is_some_and(|o| expired_at(o.started_at, o.duration, now))
        {
            self.overlay = None;
        }
        if self.overlay.is_none()
            && self
                .overlay_queue
                .front()
                .is_some_and(|o| o.started_at <= now)
        {
            self.overlay = self.overlay_queue.pop_front();
        }
    }

    fn current_cursor(&mut self, now: Instant) -> Option<Point> {
        let to = self.cursor_to?;
        if self.config.cursor.as_ref().is_some_and(|cfg| {
            matches!(cfg.motion, MotionMode::Auto)
                && self
                    .effect_active_until
                    .is_some_and(|active_until| now > active_until)
        }) {
            self.cursor = Some(to);
            self.cursor_from = Some(to);
            return None;
        }
        let from = self.cursor_from.unwrap_or(to);
        if self.cursor_duration.is_zero() {
            self.cursor = Some(to);
            self.cursor_from = Some(to);
            return Some(to);
        }
        let Some(t) = progress(self.cursor_started_at, self.cursor_duration, now) else {
            self.cursor = Some(to);
            self.cursor_from = Some(to);
            return Some(to);
        };
        let k = ease_out_cubic(t);
        let p = Point {
            x: from.x + (to.x - from.x) * k,
            y: from.y + (to.y - from.y) * k,
        };
        self.cursor = Some(p);
        Some(p)
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

#[derive(Clone, Debug)]
pub struct RecordingEffectsHandle {
    pub shared: Arc<Mutex<RecordingEffectsState>>,
}

impl RecordingEffectsHandle {
    pub async fn move_to(&self, x: f64, y: f64) {
        self.shared.lock().await.move_to(x, y);
    }

    pub async fn click(&self, x: f64, y: f64) -> Duration {
        self.shared.lock().await.click(x, y)
    }

    pub async fn click_before_dispatch(&self, x: f64, y: f64) {
        let delay = self.click(x, y).await;
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
    }

    pub async fn key(&self, label: String) {
        self.shared.lock().await.key(label);
    }
}

#[derive(Debug, Clone, Copy)]
struct Point {
    x: f64,
    y: f64,
}

impl Point {
    fn scaled(self, scale: f64) -> Self {
        Self {
            x: self.x * scale,
            y: self.y * scale,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct TimedPoint {
    point: Point,
    started_at: Instant,
    duration: Duration,
}

impl TimedPoint {
    fn new(point: Point, started_at: Instant, duration: Duration) -> Self {
        Self {
            point,
            started_at,
            duration,
        }
    }

    fn expired(self, now: Instant) -> bool {
        expired_at(self.started_at, self.duration, now)
    }
}

#[derive(Debug, Clone)]
struct TimedLabel {
    label: String,
    started_at: Instant,
    duration: Duration,
}

#[derive(Debug, Clone)]
struct TimedOverlay {
    text: String,
    position: OverlayPosition,
    started_at: Instant,
    duration: Duration,
}

impl TimedOverlay {
    fn active_until(&self) -> Instant {
        self.started_at + self.duration
    }
}

#[derive(Debug, Clone)]
struct TimedZoom {
    point: Point,
    scale: f64,
    started_at: Instant,
    duration: Option<Duration>,
}

#[derive(Debug, Clone, Copy)]
struct PointFrame {
    point: Point,
    scale: f64,
    opacity: f64,
}

#[derive(Debug, Clone)]
struct LabelFrame {
    text: String,
    opacity: f64,
    position: OverlayPosition,
}

#[derive(Debug, Clone, Copy)]
struct CameraState {
    center: Point,
    zoom: f64,
}

#[derive(Debug, Clone)]
struct FrameEffects {
    pixel_scale: f64,
    cursor: Option<(Point, CursorEffectsConfig)>,
    ripple: Option<PointFrame>,
    spotlight: Option<PointFrame>,
    key_badge: Option<LabelFrame>,
    overlay: Option<LabelFrame>,
    zoom: Option<CameraState>,
}

impl FrameEffects {
    fn has_visible_effects(&self) -> bool {
        self.cursor.is_some()
            || self.ripple.is_some()
            || self.spotlight.is_some()
            || self.key_badge.is_some()
            || self.overlay.is_some()
            || self.zoom.is_some()
    }
}

fn timed_point_state(
    timed: TimedPoint,
    now: Instant,
    hold: f64,
    max_scale: f64,
) -> Option<PointFrame> {
    let t = progress(timed.started_at, timed.duration, now)?;
    Some(PointFrame {
        point: timed.point,
        scale: 0.6 + ease_out_cubic(t) * (max_scale - 0.6),
        opacity: fade_hold(t, hold),
    })
}

fn camera_state(zoom: &TimedZoom, now: Instant) -> Option<CameraState> {
    let eased = if let Some(duration) = zoom.duration {
        let t = progress(zoom.started_at, duration, now)?;
        if t < 0.30 {
            ease_out_cubic(t / 0.30)
        } else if t > 0.78 {
            1.0 - ease_out_cubic((t - 0.78) / 0.22)
        } else {
            1.0
        }
    } else {
        let elapsed = now.checked_duration_since(zoom.started_at)?;
        ease_out_cubic((elapsed.as_secs_f64() / 0.6).clamp(0.0, 1.0))
    };
    if eased <= 0.01 {
        return None;
    }
    Some(CameraState {
        center: zoom.point,
        zoom: 1.0 + (zoom.scale - 1.0) * eased,
    })
}

fn progress(started_at: Instant, duration: Duration, now: Instant) -> Option<f64> {
    if duration.is_zero() || now < started_at {
        return None;
    }
    let elapsed = now.duration_since(started_at).as_secs_f64();
    let total = duration.as_secs_f64();
    if elapsed >= total {
        None
    } else {
        Some((elapsed / total).clamp(0.0, 1.0))
    }
}

fn expired_at(started_at: Instant, duration: Duration, now: Instant) -> bool {
    now.checked_duration_since(started_at)
        .is_some_and(|elapsed| elapsed >= duration)
}

fn fade_hold(t: f64, hold: f64) -> f64 {
    if t <= hold {
        0.90
    } else {
        0.90 * (1.0 - (t - hold) / (1.0 - hold)).clamp(0.0, 1.0)
    }
}

fn ease_out_cubic(t: f64) -> f64 {
    1.0 - (1.0 - t).powi(3)
}

fn truncate_label(label: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for ch in label.chars().take(max_chars) {
        if ch.is_control() {
            continue;
        }
        out.push(ch);
    }
    out
}

fn composite_frame(bytes: Vec<u8>, plan: FrameEffects) -> Result<Vec<u8>, Vec<u8>> {
    if !plan.has_visible_effects() {
        return Ok(bytes);
    }
    let img = image::load_from_memory(&bytes).map_err(|_| bytes.clone())?;
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return Ok(bytes);
    }

    let mut transform = CameraTransform::identity();
    let pixel_scale = plan.pixel_scale.clamp(0.1, 8.0);
    let mut canvas = if let Some(mut camera) = plan.zoom {
        camera.center = camera.center.scaled(pixel_scale);
        let (zoomed, t) = apply_zoom(img, camera);
        transform = t;
        zoomed
    } else {
        img.to_rgba8()
    };

    if let Some(spotlight) = plan.spotlight {
        draw_spotlight(
            &mut canvas,
            transform.point(spotlight.point.scaled(pixel_scale)),
            spotlight,
            pixel_scale,
        );
    }
    if let Some(ripple) = plan.ripple {
        draw_ripple(
            &mut canvas,
            transform.point(ripple.point.scaled(pixel_scale)),
            ripple,
            pixel_scale,
        );
    }
    if let Some((point, cfg)) = plan.cursor {
        let mut cfg = cfg;
        cfg.size_px = ((cfg.size_px as f64) * pixel_scale)
            .round()
            .clamp(1.0, 512.0) as u32;
        draw_cursor(
            &mut canvas,
            transform.point(point.scaled(pixel_scale)),
            &cfg,
        );
    }
    if let Some(label) = plan.key_badge {
        draw_label(&mut canvas, &label, pixel_scale);
    }
    if let Some(label) = plan.overlay {
        draw_label(&mut canvas, &label, pixel_scale);
    }

    let mut out = Cursor::new(Vec::new());
    let mut encoder = JpegEncoder::new_with_quality(&mut out, 82);
    encoder
        .encode_image(&DynamicImage::ImageRgba8(canvas))
        .map_err(|_| bytes)?;
    Ok(out.into_inner())
}

#[derive(Debug, Clone, Copy)]
struct CameraTransform {
    crop_x: f64,
    crop_y: f64,
    scale_x: f64,
    scale_y: f64,
}

impl CameraTransform {
    fn identity() -> Self {
        Self {
            crop_x: 0.0,
            crop_y: 0.0,
            scale_x: 1.0,
            scale_y: 1.0,
        }
    }

    fn point(self, point: Point) -> Point {
        Point {
            x: (point.x - self.crop_x) * self.scale_x,
            y: (point.y - self.crop_y) * self.scale_y,
        }
    }
}

fn apply_zoom(img: DynamicImage, camera: CameraState) -> (RgbaImage, CameraTransform) {
    let (w, h) = img.dimensions();
    if camera.zoom <= 1.01 {
        return (img.to_rgba8(), CameraTransform::identity());
    }

    let crop_w = ((w as f64) / camera.zoom).round().clamp(1.0, w as f64) as u32;
    let crop_h = ((h as f64) / camera.zoom).round().clamp(1.0, h as f64) as u32;
    let cx = camera.center.x.clamp(0.0, w.saturating_sub(1) as f64);
    let cy = camera.center.y.clamp(0.0, h.saturating_sub(1) as f64);
    let x = (cx - crop_w as f64 / 2.0)
        .round()
        .clamp(0.0, w.saturating_sub(crop_w) as f64) as u32;
    let y = (cy - crop_h as f64 / 2.0)
        .round()
        .clamp(0.0, h.saturating_sub(crop_h) as f64) as u32;

    let cropped = img.crop_imm(x, y, crop_w, crop_h);
    let resized =
        DynamicImage::ImageRgba8(cropped.to_rgba8()).resize_exact(w, h, FilterType::CatmullRom);
    (
        resized.to_rgba8(),
        CameraTransform {
            crop_x: x as f64,
            crop_y: y as f64,
            scale_x: w as f64 / crop_w as f64,
            scale_y: h as f64 / crop_h as f64,
        },
    )
}

fn draw_cursor(canvas: &mut RgbaImage, point: Point, cfg: &CursorEffectsConfig) {
    match cfg.theme {
        CursorTheme::Dot => {
            let r = (cfg.size_px as f64 / 2.4).max(4.0);
            draw_circle(canvas, point.x, point.y, r + 3.0, rgba(255, 255, 255, 210));
            draw_circle(canvas, point.x, point.y, r, rgba(17, 24, 39, 245));
        }
        CursorTheme::Hand => draw_hand_cursor(canvas, point, cfg.size_px as f64),
        CursorTheme::Arrow => draw_arrow_cursor(canvas, point, cfg.size_px as f64),
    }
}

fn draw_arrow_cursor(canvas: &mut RgbaImage, p: Point, size: f64) {
    let points = [
        (p.x, p.y),
        (p.x + size * 0.08, p.y + size * 0.82),
        (p.x + size * 0.27, p.y + size * 0.60),
        (p.x + size * 0.42, p.y + size * 0.95),
        (p.x + size * 0.58, p.y + size * 0.88),
        (p.x + size * 0.42, p.y + size * 0.54),
        (p.x + size * 0.70, p.y + size * 0.52),
    ];
    draw_polyline(canvas, &points, rgba(255, 255, 255, 245), 5);
    draw_polyline(canvas, &points, rgba(17, 24, 39, 250), 3);
}

fn draw_hand_cursor(canvas: &mut RgbaImage, p: Point, size: f64) {
    let dark = rgba(17, 24, 39, 250);
    let light = rgba(255, 255, 255, 235);
    draw_line(
        canvas,
        p.x,
        p.y + size * 0.18,
        p.x,
        p.y + size * 0.72,
        light,
        8,
    );
    draw_line(
        canvas,
        p.x,
        p.y + size * 0.18,
        p.x,
        p.y + size * 0.72,
        dark,
        5,
    );
    for i in 0..4 {
        let x = p.x + size * (0.12 + i as f64 * 0.13);
        draw_line(canvas, x, p.y + size * 0.35, x, p.y + size * 0.64, light, 7);
        draw_line(canvas, x, p.y + size * 0.35, x, p.y + size * 0.64, dark, 4);
    }
    draw_line(
        canvas,
        p.x - size * 0.08,
        p.y + size * 0.62,
        p.x + size * 0.42,
        p.y + size * 0.88,
        light,
        9,
    );
    draw_line(
        canvas,
        p.x - size * 0.08,
        p.y + size * 0.62,
        p.x + size * 0.42,
        p.y + size * 0.88,
        dark,
        6,
    );
}

fn draw_ripple(canvas: &mut RgbaImage, point: Point, frame: PointFrame, pixel_scale: f64) {
    let radius = (10.0 + 18.0 * frame.scale) * pixel_scale;
    let alpha = (frame.opacity * 180.0).round().clamp(0.0, 255.0) as u8;
    let outer_width = (4.0 * pixel_scale).round().clamp(1.0, 24.0) as i32;
    let inner_width = (2.0 * pixel_scale).round().clamp(1.0, 16.0) as i32;
    draw_circle_stroke(
        canvas,
        point.x,
        point.y,
        radius,
        rgba(17, 24, 39, alpha),
        outer_width,
    );
    draw_circle_stroke(
        canvas,
        point.x,
        point.y,
        radius + 3.0,
        rgba(255, 255, 255, alpha.saturating_sub(40)),
        inner_width,
    );
}

fn draw_spotlight(canvas: &mut RgbaImage, point: Point, frame: PointFrame, pixel_scale: f64) {
    let (w, h) = canvas.dimensions();
    let dim = rgba(
        0,
        0,
        0,
        (frame.opacity * 88.0).round().clamp(0.0, 255.0) as u8,
    );
    draw_rect(canvas, 0, 0, w as i32, h as i32, dim);
    let radius = 44.0 * frame.scale * pixel_scale;
    clear_circle(canvas, point.x, point.y, radius);
    draw_circle_stroke(
        canvas,
        point.x,
        point.y,
        radius,
        rgba(14, 165, 233, (frame.opacity * 250.0).round() as u8),
        (5.0 * pixel_scale).round().clamp(1.0, 32.0) as i32,
    );
}

fn draw_label(canvas: &mut RgbaImage, label: &LabelFrame, pixel_scale: f64) {
    if label.text.is_empty() || label.opacity <= 0.01 {
        return;
    }
    let (w, h) = canvas.dimensions();
    let base_scale = if matches!(label.position, OverlayPosition::Top) {
        2
    } else {
        3
    };
    let scale = ((base_scale as f64) * pixel_scale).round().clamp(1.0, 12.0) as i32;
    let text_w = text_width(&label.text, scale);
    let text_h = 7 * scale;
    let pad_x = (14.0 * pixel_scale).round().clamp(6.0, 80.0) as i32;
    let pad_y = (9.0 * pixel_scale).round().clamp(4.0, 60.0) as i32;
    let box_w = (text_w + pad_x * 2).min(w.saturating_sub(20) as i32);
    let box_h = text_h + pad_y * 2;
    let margin = (8.0 * pixel_scale).round().clamp(4.0, 80.0) as i32;
    let x = ((w as i32 - box_w) / 2).max(margin);
    let y = match label.position {
        OverlayPosition::Top => (24.0 * pixel_scale).round().max(margin as f64) as i32,
        OverlayPosition::Center => ((h as i32 - box_h) / 2).max(margin),
        OverlayPosition::Bottom => {
            (h as i32 - box_h - (32.0 * pixel_scale).round().max(margin as f64) as i32).max(margin)
        }
    };
    let bg_alpha = (label.opacity * 226.0).round().clamp(0.0, 255.0) as u8;
    draw_rect(canvas, x, y, box_w, box_h, rgba(17, 24, 39, bg_alpha));
    draw_rect_stroke(
        canvas,
        x,
        y,
        box_w,
        box_h,
        rgba(255, 255, 255, bg_alpha / 3),
        1,
    );
    draw_text(
        canvas,
        &label.text,
        x + pad_x,
        y + pad_y,
        scale,
        rgba(255, 255, 255, (label.opacity * 255.0).round() as u8),
    );
}

fn draw_text(canvas: &mut RgbaImage, text: &str, x: i32, y: i32, scale: i32, color: Rgba<u8>) {
    let mut cx = x;
    for ch in text.chars() {
        if ch == ' ' {
            cx += 4 * scale;
            continue;
        }
        let glyph = glyph_pattern(ch);
        for (row, bits) in glyph.iter().enumerate() {
            for (col, bit) in bits.chars().enumerate() {
                if bit == '1' {
                    draw_rect(
                        canvas,
                        cx + col as i32 * scale,
                        y + row as i32 * scale,
                        scale,
                        scale,
                        color,
                    );
                }
            }
        }
        cx += 6 * scale;
    }
}

fn text_width(text: &str, scale: i32) -> i32 {
    text.chars()
        .map(|ch| if ch == ' ' { 4 } else { 6 })
        .sum::<i32>()
        * scale
}

fn glyph_pattern(ch: char) -> [&'static str; 7] {
    match ch.to_ascii_uppercase() {
        'A' => [
            "01110", "10001", "10001", "11111", "10001", "10001", "10001",
        ],
        'B' => [
            "11110", "10001", "10001", "11110", "10001", "10001", "11110",
        ],
        'C' => [
            "01111", "10000", "10000", "10000", "10000", "10000", "01111",
        ],
        'D' => [
            "11110", "10001", "10001", "10001", "10001", "10001", "11110",
        ],
        'E' => [
            "11111", "10000", "10000", "11110", "10000", "10000", "11111",
        ],
        'F' => [
            "11111", "10000", "10000", "11110", "10000", "10000", "10000",
        ],
        'G' => [
            "01111", "10000", "10000", "10011", "10001", "10001", "01111",
        ],
        'H' => [
            "10001", "10001", "10001", "11111", "10001", "10001", "10001",
        ],
        'I' => [
            "11111", "00100", "00100", "00100", "00100", "00100", "11111",
        ],
        'J' => [
            "00111", "00010", "00010", "00010", "10010", "10010", "01100",
        ],
        'K' => [
            "10001", "10010", "10100", "11000", "10100", "10010", "10001",
        ],
        'L' => [
            "10000", "10000", "10000", "10000", "10000", "10000", "11111",
        ],
        'M' => [
            "10001", "11011", "10101", "10101", "10001", "10001", "10001",
        ],
        'N' => [
            "10001", "11001", "10101", "10011", "10001", "10001", "10001",
        ],
        'O' => [
            "01110", "10001", "10001", "10001", "10001", "10001", "01110",
        ],
        'P' => [
            "11110", "10001", "10001", "11110", "10000", "10000", "10000",
        ],
        'Q' => [
            "01110", "10001", "10001", "10001", "10101", "10010", "01101",
        ],
        'R' => [
            "11110", "10001", "10001", "11110", "10100", "10010", "10001",
        ],
        'S' => [
            "01111", "10000", "10000", "01110", "00001", "00001", "11110",
        ],
        'T' => [
            "11111", "00100", "00100", "00100", "00100", "00100", "00100",
        ],
        'U' => [
            "10001", "10001", "10001", "10001", "10001", "10001", "01110",
        ],
        'V' => [
            "10001", "10001", "10001", "10001", "10001", "01010", "00100",
        ],
        'W' => [
            "10001", "10001", "10001", "10101", "10101", "10101", "01010",
        ],
        'X' => [
            "10001", "10001", "01010", "00100", "01010", "10001", "10001",
        ],
        'Y' => [
            "10001", "10001", "01010", "00100", "00100", "00100", "00100",
        ],
        'Z' => [
            "11111", "00001", "00010", "00100", "01000", "10000", "11111",
        ],
        '0' => [
            "01110", "10001", "10011", "10101", "11001", "10001", "01110",
        ],
        '1' => [
            "00100", "01100", "00100", "00100", "00100", "00100", "01110",
        ],
        '2' => [
            "01110", "10001", "00001", "00010", "00100", "01000", "11111",
        ],
        '3' => [
            "11110", "00001", "00001", "01110", "00001", "00001", "11110",
        ],
        '4' => [
            "00010", "00110", "01010", "10010", "11111", "00010", "00010",
        ],
        '5' => [
            "11111", "10000", "10000", "11110", "00001", "00001", "11110",
        ],
        '6' => [
            "01110", "10000", "10000", "11110", "10001", "10001", "01110",
        ],
        '7' => [
            "11111", "00001", "00010", "00100", "01000", "01000", "01000",
        ],
        '8' => [
            "01110", "10001", "10001", "01110", "10001", "10001", "01110",
        ],
        '9' => [
            "01110", "10001", "10001", "01111", "00001", "00001", "01110",
        ],
        '.' => [
            "00000", "00000", "00000", "00000", "00000", "01100", "01100",
        ],
        ',' => [
            "00000", "00000", "00000", "00000", "01100", "00100", "01000",
        ],
        ':' => [
            "00000", "01100", "01100", "00000", "01100", "01100", "00000",
        ],
        '-' => [
            "00000", "00000", "00000", "11111", "00000", "00000", "00000",
        ],
        '_' => [
            "00000", "00000", "00000", "00000", "00000", "00000", "11111",
        ],
        '/' => [
            "00001", "00010", "00010", "00100", "01000", "01000", "10000",
        ],
        '@' => [
            "01110", "10001", "10111", "10101", "10111", "10000", "01110",
        ],
        _ => [
            "11111", "10001", "00110", "00100", "00110", "10001", "11111",
        ],
    }
}

fn draw_polyline(canvas: &mut RgbaImage, points: &[(f64, f64)], color: Rgba<u8>, width: i32) {
    for pair in points.windows(2) {
        draw_line(
            canvas, pair[0].0, pair[0].1, pair[1].0, pair[1].1, color, width,
        );
    }
    if let (Some(first), Some(last)) = (points.first(), points.last()) {
        draw_line(canvas, last.0, last.1, first.0, first.1, color, width);
    }
}

fn draw_line(
    canvas: &mut RgbaImage,
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    color: Rgba<u8>,
    width: i32,
) {
    let dx = x1 - x0;
    let dy = y1 - y0;
    let steps = dx.abs().max(dy.abs()).round().max(1.0) as i32;
    for i in 0..=steps {
        let t = i as f64 / steps as f64;
        let x = x0 + dx * t;
        let y = y0 + dy * t;
        draw_circle(canvas, x, y, width as f64 / 2.0, color);
    }
}

fn draw_circle(canvas: &mut RgbaImage, cx: f64, cy: f64, radius: f64, color: Rgba<u8>) {
    let r = radius.ceil() as i32;
    let cx_i = cx.round() as i32;
    let cy_i = cy.round() as i32;
    let r2 = radius * radius;
    for y in (cy_i - r)..=(cy_i + r) {
        for x in (cx_i - r)..=(cx_i + r) {
            let dx = x as f64 - cx;
            let dy = y as f64 - cy;
            if dx * dx + dy * dy <= r2 {
                blend_pixel(canvas, x, y, color);
            }
        }
    }
}

fn clear_circle(canvas: &mut RgbaImage, cx: f64, cy: f64, radius: f64) {
    let r = radius.ceil() as i32;
    let cx_i = cx.round() as i32;
    let cy_i = cy.round() as i32;
    let r2 = radius * radius;
    for y in (cy_i - r)..=(cy_i + r) {
        for x in (cx_i - r)..=(cx_i + r) {
            let dx = x as f64 - cx;
            let dy = y as f64 - cy;
            if dx * dx + dy * dy <= r2 {
                let Some(pixel) = pixel_mut_checked(canvas, x, y) else {
                    continue;
                };
                let lift = 18;
                pixel.0[0] = pixel.0[0].saturating_add(lift);
                pixel.0[1] = pixel.0[1].saturating_add(lift);
                pixel.0[2] = pixel.0[2].saturating_add(lift);
            }
        }
    }
}

fn draw_circle_stroke(
    canvas: &mut RgbaImage,
    cx: f64,
    cy: f64,
    radius: f64,
    color: Rgba<u8>,
    width: i32,
) {
    let outer = radius + width as f64 / 2.0;
    let inner = (radius - width as f64 / 2.0).max(0.0);
    let r = outer.ceil() as i32;
    let cx_i = cx.round() as i32;
    let cy_i = cy.round() as i32;
    let outer2 = outer * outer;
    let inner2 = inner * inner;
    for y in (cy_i - r)..=(cy_i + r) {
        for x in (cx_i - r)..=(cx_i + r) {
            let dx = x as f64 - cx;
            let dy = y as f64 - cy;
            let d2 = dx * dx + dy * dy;
            if d2 <= outer2 && d2 >= inner2 {
                blend_pixel(canvas, x, y, color);
            }
        }
    }
}

fn draw_rect(canvas: &mut RgbaImage, x: i32, y: i32, w: i32, h: i32, color: Rgba<u8>) {
    if w <= 0 || h <= 0 {
        return;
    }
    let (canvas_w, canvas_h) = canvas.dimensions();
    let x0 = x.clamp(0, canvas_w as i32) as u32;
    let y0 = y.clamp(0, canvas_h as i32) as u32;
    let x1 = (x + w).clamp(0, canvas_w as i32) as u32;
    let y1 = (y + h).clamp(0, canvas_h as i32) as u32;
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    for yy in y0..y1 {
        for xx in x0..x1 {
            blend_pixel_unchecked(canvas.get_pixel_mut(xx, yy), color);
        }
    }
}

fn draw_rect_stroke(
    canvas: &mut RgbaImage,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    color: Rgba<u8>,
    width: i32,
) {
    draw_rect(canvas, x, y, w, width, color);
    draw_rect(canvas, x, y + h - width, w, width, color);
    draw_rect(canvas, x, y, width, h, color);
    draw_rect(canvas, x + w - width, y, width, h, color);
}

fn blend_pixel(canvas: &mut RgbaImage, x: i32, y: i32, src: Rgba<u8>) {
    let Some(dst) = pixel_mut_checked(canvas, x, y) else {
        return;
    };
    blend_pixel_unchecked(dst, src);
}

fn blend_pixel_unchecked(dst: &mut Rgba<u8>, src: Rgba<u8>) {
    let alpha = src.0[3] as f32 / 255.0;
    let inv = 1.0 - alpha;
    dst.0[0] = (src.0[0] as f32 * alpha + dst.0[0] as f32 * inv).round() as u8;
    dst.0[1] = (src.0[1] as f32 * alpha + dst.0[1] as f32 * inv).round() as u8;
    dst.0[2] = (src.0[2] as f32 * alpha + dst.0[2] as f32 * inv).round() as u8;
    dst.0[3] = 255;
}

fn pixel_mut_checked(canvas: &mut RgbaImage, x: i32, y: i32) -> Option<&mut Rgba<u8>> {
    if x < 0 || y < 0 {
        return None;
    }
    let (w, h) = canvas.dimensions();
    let x = x as u32;
    let y = y as u32;
    if x >= w || y >= h {
        return None;
    }
    Some(canvas.get_pixel_mut(x, y))
}

fn rgba(r: u8, g: u8, b: u8, a: u8) -> Rgba<u8> {
    Rgba([r, g, b, a])
}

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
        assert_eq!(cfg.cursor.unwrap().click_sync, ClickSync::Block);
    }

    #[test]
    fn click_does_not_auto_zoom_or_spotlight() {
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

        state.click(120.0, 80.0);

        assert!(state.ripple.is_some());
        assert!(state.zoom.is_none());
        assert!(state.spotlight.is_none());
    }

    #[test]
    fn explicit_zoom_produces_camera_frame() {
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
        state.zoom_to(120.0, 80.0, 1.6, Some(1200));
        state.zoom.as_mut().unwrap().started_at = Instant::now() - Duration::from_millis(400);

        let plan = state.frame_plan(Instant::now());

        assert!(plan.zoom.unwrap().zoom > 1.1);
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
    fn zoom_without_duration_holds_until_reset() {
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
        state.zoom.as_mut().unwrap().started_at = Instant::now() - Duration::from_secs(3);

        let plan = state.frame_plan(Instant::now());

        assert!(plan.zoom.unwrap().zoom > 1.5);
        assert!(state.zoom.is_some());
        state.zoom_reset();
        assert!(state.frame_plan(Instant::now()).zoom.is_none());
    }

    #[test]
    fn overlays_serialize_instead_of_replacing() {
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
        state.overlay_text("second".to_string(), OverlayPosition::Bottom, 1_000);
        let now = state.overlay.as_ref().unwrap().started_at;

        let first = state.frame_plan(now).overlay.unwrap();
        assert_eq!(first.text, "first");

        let second = state
            .frame_plan(now + Duration::from_millis(1_050))
            .overlay
            .unwrap();
        assert_eq!(second.text, "second");
    }

    #[test]
    fn auto_cursor_hides_after_effect_timeline_but_always_cursor_persists() {
        let auto_cfg = RecordingEffectsConfig::from_cmd(
            RecordEffectsPreset::Cursor,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap()
        .unwrap();
        let mut auto_state = RecordingEffectsState::new(auto_cfg);
        auto_state.move_to(120.0, 80.0);
        let active_until = auto_state.effect_active_until.unwrap();

        assert!(auto_state
            .frame_plan(active_until + Duration::from_millis(1))
            .cursor
            .is_none());

        let always_cursor = serde_json::json!({
            "theme": "arrow",
            "motion": "always"
        });
        let always_cfg = RecordingEffectsConfig::from_cmd(
            RecordEffectsPreset::Cursor,
            Some(&always_cursor),
            None,
            None,
            None,
            None,
        )
        .unwrap()
        .unwrap();
        let mut always_state = RecordingEffectsState::new(always_cfg);
        always_state.move_to(120.0, 80.0);
        let active_until = always_state.effect_active_until.unwrap();

        assert!(always_state
            .frame_plan(active_until + Duration::from_millis(1))
            .cursor
            .is_some());
    }

    #[test]
    fn compositor_changes_frame_when_cursor_is_visible() {
        let mut img = RgbaImage::from_pixel(120, 80, rgba(255, 255, 255, 255));
        let mut raw = Cursor::new(Vec::new());
        JpegEncoder::new_with_quality(&mut raw, 90)
            .encode_image(&DynamicImage::ImageRgba8(img.clone()))
            .unwrap();
        let plan = FrameEffects {
            pixel_scale: 1.0,
            cursor: Some((Point { x: 30.0, y: 20.0 }, CursorEffectsConfig::default())),
            ripple: None,
            spotlight: None,
            key_badge: None,
            overlay: None,
            zoom: None,
        };

        let out = composite_frame(raw.into_inner(), plan).unwrap();
        let decoded = image::load_from_memory(&out).unwrap().to_rgba8();

        assert_ne!(img.get_pixel(30, 20), decoded.get_pixel(30, 20));
        img.put_pixel(30, 20, *decoded.get_pixel(30, 20));
    }

    #[test]
    fn compositor_changes_frame_when_overlay_is_visible() {
        let img = RgbaImage::from_pixel(240, 160, rgba(255, 255, 255, 255));
        let mut raw = Cursor::new(Vec::new());
        JpegEncoder::new_with_quality(&mut raw, 90)
            .encode_image(&DynamicImage::ImageRgba8(img.clone()))
            .unwrap();
        let plan = FrameEffects {
            pixel_scale: 1.0,
            cursor: None,
            ripple: None,
            spotlight: None,
            key_badge: None,
            overlay: Some(LabelFrame {
                text: "overlay".to_string(),
                opacity: 0.9,
                position: OverlayPosition::Center,
            }),
            zoom: None,
        };

        let out = composite_frame(raw.into_inner(), plan).unwrap();
        let decoded = image::load_from_memory(&out).unwrap().to_rgba8();

        assert_ne!(img.get_pixel(120, 80), decoded.get_pixel(120, 80));
    }

    #[test]
    fn compositor_changes_frame_when_zoom_is_visible() {
        let mut img = RgbaImage::from_pixel(240, 160, rgba(255, 255, 255, 255));
        for x in 0..240 {
            for y in 0..160 {
                img.put_pixel(x, y, rgba(x as u8, y as u8, 128, 255));
            }
        }
        let mut raw = Cursor::new(Vec::new());
        JpegEncoder::new_with_quality(&mut raw, 90)
            .encode_image(&DynamicImage::ImageRgba8(img.clone()))
            .unwrap();
        let plan = FrameEffects {
            pixel_scale: 1.0,
            cursor: None,
            ripple: None,
            spotlight: None,
            key_badge: None,
            overlay: None,
            zoom: Some(CameraState {
                center: Point { x: 180.0, y: 120.0 },
                zoom: 1.6,
            }),
        };

        let out = composite_frame(raw.into_inner(), plan).unwrap();
        let decoded = image::load_from_memory(&out).unwrap().to_rgba8();

        assert_ne!(img.get_pixel(20, 20), decoded.get_pixel(20, 20));
    }

    #[tokio::test]
    async fn process_frame_uses_shared_overlay_and_zoom_state() {
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
        let shared = Arc::new(Mutex::new(RecordingEffectsState::new(cfg)));

        let img = RgbaImage::from_pixel(240, 160, rgba(255, 255, 255, 255));
        let mut raw = Cursor::new(Vec::new());
        JpegEncoder::new_with_quality(&mut raw, 90)
            .encode_image(&DynamicImage::ImageRgba8(img.clone()))
            .unwrap();
        let raw = raw.into_inner();

        shared
            .lock()
            .await
            .overlay_text("overlay".to_string(), OverlayPosition::Center, 5_000);
        let overlay_out = RecordingEffectsState::process_frame(&shared, raw.clone()).await;
        let overlay_decoded = image::load_from_memory(&overlay_out).unwrap().to_rgba8();
        assert_ne!(img.get_pixel(120, 80), overlay_decoded.get_pixel(120, 80));

        {
            let mut guard = shared.lock().await;
            guard.clear_overlay();
            guard.zoom_to(180.0, 120.0, 1.6, None);
            guard.zoom.as_mut().unwrap().started_at = Instant::now() - Duration::from_secs(1);
        }
        let mut gradient = RgbaImage::from_pixel(240, 160, rgba(255, 255, 255, 255));
        for x in 0..240 {
            for y in 0..160 {
                gradient.put_pixel(x, y, rgba(x as u8, y as u8, 128, 255));
            }
        }
        let mut gradient_raw = Cursor::new(Vec::new());
        JpegEncoder::new_with_quality(&mut gradient_raw, 90)
            .encode_image(&DynamicImage::ImageRgba8(gradient.clone()))
            .unwrap();
        let zoom_out =
            RecordingEffectsState::process_frame(&shared, gradient_raw.into_inner()).await;
        let zoom_decoded = image::load_from_memory(&zoom_out).unwrap().to_rgba8();
        assert_ne!(gradient.get_pixel(20, 20), zoom_decoded.get_pixel(20, 20));
    }
}
