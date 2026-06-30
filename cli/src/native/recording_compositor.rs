use base64::Engine;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::sync::oneshot;

use super::browser::RECORDING_COMPOSITOR_URL;
use super::cdp::client::CdpClient;
use super::cdp::types::{
    AttachToTargetParams, AttachToTargetResult, CaptureScreenshotParams, CaptureScreenshotResult,
    CreateTargetResult, EvaluateParams, EvaluateResult,
};
use super::recording::{self, RecordingCaptureGate};

#[derive(Clone)]
pub struct RecordingCompositor {
    pub client: Arc<CdpClient>,
    pub target_id: String,
    pub session_id: String,
}

impl RecordingCompositor {
    pub async fn create(
        client: Arc<CdpClient>,
        browser_context_id: Option<&str>,
        width: i32,
        height: i32,
        device_scale_factor: f64,
        mobile: bool,
    ) -> Result<Self, String> {
        let mut params = json!({ "url": RECORDING_COMPOSITOR_URL });
        if let Some(context_id) = browser_context_id {
            params["browserContextId"] = json!(context_id);
        }
        let create_result: CreateTargetResult = client
            .send_command_typed("Target.createTarget", &params, None)
            .await?;
        let attach_result: AttachToTargetResult = client
            .send_command_typed(
                "Target.attachToTarget",
                &AttachToTargetParams {
                    target_id: create_result.target_id.clone(),
                    flatten: true,
                },
                None,
            )
            .await?;
        let compositor = Self {
            client,
            target_id: create_result.target_id,
            session_id: attach_result.session_id,
        };
        compositor
            .enable(width, height, device_scale_factor, mobile)
            .await?;
        compositor.install().await?;
        compositor.activate().await;
        Ok(compositor)
    }

    async fn enable(
        &self,
        width: i32,
        height: i32,
        device_scale_factor: f64,
        mobile: bool,
    ) -> Result<(), String> {
        self.client
            .send_command_no_params("Page.enable", Some(&self.session_id))
            .await?;
        self.client
            .send_command_no_params("Runtime.enable", Some(&self.session_id))
            .await?;
        let _ = self
            .client
            .send_command_no_params("Runtime.runIfWaitingForDebugger", Some(&self.session_id))
            .await;
        self.client
            .send_command(
                "Emulation.setDeviceMetricsOverride",
                Some(json!({
                    "width": width.max(1),
                    "height": height.max(1),
                    "deviceScaleFactor": device_scale_factor.max(0.1),
                    "mobile": mobile,
                })),
                Some(&self.session_id),
            )
            .await?;
        Ok(())
    }

    async fn install(&self) -> Result<(), String> {
        self.evaluate(format!(
            "document.open(); document.write({}); document.close();",
            js_string(COMPOSITOR_HTML)
        ))
        .await?;
        self.evaluate(COMPOSITOR_RUNTIME_JS.to_string()).await
    }

    pub async fn close(&self) {
        let _ = self
            .client
            .send_command(
                "Target.closeTarget",
                Some(json!({ "targetId": self.target_id })),
                None,
            )
            .await;
    }

    async fn activate(&self) {
        let _ = self
            .client
            .send_command(
                "Target.activateTarget",
                Some(json!({ "targetId": self.target_id })),
                None,
            )
            .await;
    }

    async fn set_frame(&self, jpeg_base64: &str) -> Result<(), String> {
        self.evaluate(format!(
            "window.__agentBrowserRecordingCompositor.setFrame({})",
            js_string(jpeg_base64)
        ))
        .await
    }

    async fn evaluate(&self, expression: String) -> Result<(), String> {
        let result: EvaluateResult = self
            .client
            .send_command_typed(
                "Runtime.evaluate",
                &EvaluateParams {
                    expression,
                    return_by_value: Some(true),
                    await_promise: Some(true),
                },
                Some(&self.session_id),
            )
            .await?;
        if let Some(details) = result.exception_details {
            let msg = details
                .exception
                .as_ref()
                .and_then(|e| e.description.as_deref())
                .unwrap_or(&details.text);
            return Err(format!("Recording compositor runtime error: {}", msg));
        }
        Ok(())
    }
}

pub fn spawn_compositor_recording_task(
    client: Arc<CdpClient>,
    source_session_id: String,
    compositor: RecordingCompositor,
    output_path: String,
    fps: u32,
    shared_count: Arc<AtomicU64>,
    cancel_rx: oneshot::Receiver<()>,
    capture_gate: Option<Arc<RecordingCaptureGate>>,
) -> tokio::task::JoinHandle<Result<(), String>> {
    tokio::spawn(async move {
        let mut cancel_rx = std::pin::pin!(cancel_rx);

        let mut ffmpeg = recording::build_ffmpeg_command(&output_path, fps)
            .spawn()
            .map_err(|e| {
                format!(
                "ffmpeg not found or failed to execute: {}. Install ffmpeg to enable recording.",
                e
            )
            })?;

        let mut stdin = ffmpeg
            .stdin
            .take()
            .ok_or_else(|| "Failed to open ffmpeg stdin".to_string())?;

        let mut interval = tokio::time::interval(recording::capture_interval(fps));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut started_at = None;
        let mut segment_frames_written = 0_u64;
        let mut last_source_frame: Option<String> = None;
        let mut last_source_refresh = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .unwrap_or_else(Instant::now);

        let source_params = CaptureScreenshotParams {
            format: Some("jpeg".to_string()),
            quality: Some(82),
            clip: None,
            from_surface: Some(true),
            capture_beyond_viewport: None,
        };
        let compositor_params = CaptureScreenshotParams {
            format: Some("jpeg".to_string()),
            quality: Some(90),
            clip: None,
            from_surface: Some(true),
            capture_beyond_viewport: None,
        };

        loop {
            if let Some(ref gate) = capture_gate {
                if !gate.is_active().await {
                    started_at = None;
                    segment_frames_written = 0;
                    tokio::select! {
                        _ = &mut cancel_rx => break,
                        _ = gate.wait_until_active() => {}
                    }
                    interval.reset();
                }
            }

            tokio::select! {
                _ = &mut cancel_rx => break,
                _ = interval.tick() => {}
            }

            if let Some(ref gate) = capture_gate {
                if !gate.is_active().await {
                    started_at = None;
                    segment_frames_written = 0;
                    continue;
                }
            }

            if last_source_refresh.elapsed() >= Duration::from_millis(120) {
                last_source_refresh = Instant::now();
                let source_update = async {
                    let source = client
                        .send_command_typed::<_, CaptureScreenshotResult>(
                            "Page.captureScreenshot",
                            &source_params,
                            Some(&source_session_id),
                        )
                        .await?;
                    if last_source_frame.as_deref() != Some(source.data.as_str()) {
                        compositor.set_frame(&source.data).await?;
                        last_source_frame = Some(source.data);
                    }
                    Ok::<(), String>(())
                };
                let _ = tokio::time::timeout(Duration::from_millis(90), source_update).await;
                if last_source_frame.is_none() {
                    continue;
                }
            }

            compositor.activate().await;
            let screenshot = match compositor
                .client
                .send_command_typed::<_, CaptureScreenshotResult>(
                    "Page.captureScreenshot",
                    &compositor_params,
                    Some(&compositor.session_id),
                )
                .await
            {
                Ok(s) => s,
                Err(_) => {
                    continue;
                }
            };

            let bytes = match base64::engine::general_purpose::STANDARD.decode(&screenshot.data) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let now = Instant::now();
            let capture_started_at = *started_at.get_or_insert(now);
            let due_frames =
                recording::due_frame_count(capture_started_at, now, fps, segment_frames_written);
            let mut write_failed = false;
            for _ in 0..due_frames {
                if stdin.write_all(&bytes).await.is_err() {
                    write_failed = true;
                    break;
                }
                shared_count.fetch_add(1, Ordering::Relaxed);
                segment_frames_written += 1;
            }
            if write_failed {
                break;
            }
        }

        drop(stdin);
        compositor.close().await;

        let output = ffmpeg
            .wait_with_output()
            .await
            .map_err(|e| format!("ffmpeg wait failed: {}", e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("ffmpeg failed: {}", stderr));
        }

        Ok(())
    })
}

pub async fn viewport_for_session(
    client: &CdpClient,
    session_id: &str,
    fallback: Option<(i32, i32, f64, bool)>,
) -> (i32, i32, f64, bool) {
    if let Some(viewport) = fallback {
        return viewport;
    }
    let expression = r#"({
      width: Math.max(1, window.innerWidth || document.documentElement.clientWidth || 1280),
      height: Math.max(1, window.innerHeight || document.documentElement.clientHeight || 720),
      deviceScaleFactor: window.devicePixelRatio || 1,
      mobile: /Mobi|Android/i.test(navigator.userAgent)
    })"#;
    let result: Result<EvaluateResult, String> = client
        .send_command_typed(
            "Runtime.evaluate",
            &EvaluateParams {
                expression: expression.to_string(),
                return_by_value: Some(true),
                await_promise: Some(false),
            },
            Some(session_id),
        )
        .await;
    let value = result.ok().and_then(|r| r.result.value);
    let width = value
        .as_ref()
        .and_then(|v| v.get("width"))
        .and_then(Value::as_i64)
        .unwrap_or(1280) as i32;
    let height = value
        .as_ref()
        .and_then(|v| v.get("height"))
        .and_then(Value::as_i64)
        .unwrap_or(720) as i32;
    let scale = value
        .as_ref()
        .and_then(|v| v.get("deviceScaleFactor"))
        .and_then(Value::as_f64)
        .unwrap_or(1.0);
    let mobile = value
        .as_ref()
        .and_then(|v| v.get("mobile"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    (width, height, scale, mobile)
}

fn js_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

const COMPOSITOR_HTML: &str = r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<style>
html, body {
  width: 100%;
  height: 100%;
  margin: 0;
  overflow: hidden;
  background: #fff;
}
body {
  font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
}
#stage {
  position: fixed;
  inset: 0;
  overflow: hidden;
  background: #fff;
}
#camera {
  position: absolute;
  inset: 0;
  transform-origin: 0 0;
  transform: scale(1);
  will-change: transform;
}
#content-frame {
  position: absolute;
  inset: 0;
  width: 100%;
  height: 100%;
  object-fit: fill;
  display: block;
}
#effects {
  position: fixed;
  inset: 0;
  pointer-events: none;
  overflow: hidden;
  z-index: 2147483647;
}
</style>
</head>
<body>
<div id="stage">
  <div id="camera"><img id="content-frame" alt=""></div>
  <div id="effects"></div>
</div>
</body>
</html>"#;

const COMPOSITOR_RUNTIME_JS: &str = r#"
(() => {
  const VERSION = 1;
  if (window.__agentBrowserRecordingCompositor?.version === VERSION) return;

  const camera = document.getElementById('camera');
  const frame = document.getElementById('content-frame');
  const effects = document.getElementById('effects');
  const ns = 'http://www.w3.org/2000/svg';

  const state = {
    cursor: { theme: 'arrow', size: 28, tweenMs: 700, clickMs: 500, motion: 'always', clickSync: 'block' },
    camera: { scale: 1, originX: 0, originY: 0 },
    cursorPoint: null,
    cursorAnimation: null,
    cursorGeneration: 0,
    overlayChain: Promise.resolve(),
    overlayGeneration: 0,
    zoomGeneration: 0,
    zoomTimer: null,
  };

  let cursor = null;
  let cursorPath = null;

  function cursorShape(theme) {
    if (theme === 'dot') {
      return { viewBox: '0 0 24 24', tipX: .5, tipY: .5, path: '<circle cx="12" cy="12" r="6"/>' };
    }
    if (theme === 'hand') {
      return { viewBox: '0 0 32 32', tipX: .42, tipY: .12, path: '<path d="M12.8 3.4c1.3 0 2.3 1 2.3 2.3v8.1h.6V8.2a2.2 2.2 0 0 1 4.4 0v6.1h.6v-4a2.2 2.2 0 0 1 4.4 0v6.4l.8-1.6a2.1 2.1 0 0 1 3.8 1.8l-2.9 6.2c-1.5 3.2-4.8 5.3-8.4 5.3h-3.1c-2.5 0-4.9-1-6.6-2.8L3.6 16a2.2 2.2 0 0 1 3.1-3.1l3.7 3.6V5.7c0-1.3 1.1-2.3 2.4-2.3z"/>' };
    }
    return { viewBox: '0 0 32 32', tipX: .18, tipY: .12, path: '<path d="M5 3.8 26.8 17 17 19.2 12 28.5 5 3.8z"/>' };
  }

  function screenPoint(point) {
    const c = state.camera;
    return {
      x: c.originX + (point.x - c.originX) * c.scale,
      y: c.originY + (point.y - c.originY) * c.scale,
    };
  }

  function screenRadius(radius) {
    return Math.max(24, radius * state.camera.scale);
  }

  function cursorTransform(point) {
    const size = Math.max(8, Math.min(96, Number(state.cursor?.size) || 28));
    const tipX = Number(cursor?.dataset.tipX || size * .18);
    const tipY = Number(cursor?.dataset.tipY || size * .12);
    return `translate3d(${point.x - tipX}px, ${point.y - tipY}px, 0)`;
  }

  function idleCursorPoint() {
    const size = Math.max(8, Math.min(96, Number(state.cursor?.size) || 28));
    const margin = Math.max(36, size * .9);
    return { x: margin, y: Math.max(margin, window.innerHeight - margin) };
  }

  function ensureCursor() {
    if (!state.cursor || state.cursor.motion === 'off') return null;
    if (cursor?.isConnected) return cursor;
    cursor = document.createElementNS(ns, 'svg');
    cursor.setAttribute('data-agent-browser-recording-cursor', '');
    Object.assign(cursor.style, {
      position: 'absolute',
      left: '0',
      top: '0',
      opacity: '1',
      fill: '#fff',
      stroke: 'rgba(0,0,0,.82)',
      strokeWidth: '1.25',
      strokeLinejoin: 'round',
      paintOrder: 'stroke fill',
      filter: 'drop-shadow(0 2px 2px rgba(0,0,0,.55))',
      zIndex: '60',
      pointerEvents: 'none',
      willChange: 'transform, opacity',
      transformOrigin: '0 0',
    });
    cursorPath = document.createElementNS(ns, 'g');
    cursor.appendChild(cursorPath);
    effects.appendChild(cursor);
    updateCursorShape();
    const point = state.cursorPoint || idleCursorPoint();
    state.cursorPoint = point;
    cursor.style.transform = cursorTransform(point);
    return cursor;
  }

  function updateCursorShape() {
    if (!cursor || !state.cursor) return;
    const size = Math.max(8, Math.min(96, Number(state.cursor.size) || 28));
    const shape = cursorShape(state.cursor.theme);
    cursor.setAttribute('viewBox', shape.viewBox);
    cursor.setAttribute('width', String(size));
    cursor.setAttribute('height', String(size));
    cursor.dataset.tipX = String(shape.tipX * size);
    cursor.dataset.tipY = String(shape.tipY * size);
    cursorPath.innerHTML = shape.path;
  }

  async function moveTo(x, y, options = {}) {
    if (!state.cursor || state.cursor.motion === 'off') return;
    const el = ensureCursor();
    if (!el) return;
    updateCursorShape();
    const contentPoint = { x: Number(x) || 0, y: Number(y) || 0 };
    const to = screenPoint(contentPoint);
    const from = state.cursorPoint || idleCursorPoint();
    const duration = Math.max(0, Number(options.durationMs ?? state.cursor.tweenMs) || 0);
    const generation = ++state.cursorGeneration;
    state.cursorAnimation?.cancel();
    if (duration === 0) {
      el.style.opacity = '1';
      el.style.transform = cursorTransform(to);
      state.cursorPoint = to;
      return;
    }
    el.style.opacity = '1';
    el.style.transform = cursorTransform(from);
    state.cursorAnimation = el.animate(
      [
        { transform: cursorTransform(from), opacity: 1 },
        { transform: cursorTransform(to), opacity: 1 },
      ],
      { duration, easing: 'cubic-bezier(0.45, 0, 0.15, 1)', fill: 'forwards' }
    );
    await Promise.all([
      state.cursorAnimation.finished.catch(() => {}),
      new Promise(resolve => setTimeout(resolve, duration)),
    ]);
    if (generation !== state.cursorGeneration) return;
    el.style.opacity = '1';
    el.style.transform = cursorTransform(to);
    state.cursorPoint = to;
  }

  function burstAt(point, duration) {
    const ms = Math.max(260, Number(duration) || 500);
    const group = document.createElement('div');
    group.style.position = 'absolute';
    group.style.left = `${point.x}px`;
    group.style.top = `${point.y}px`;
    group.style.width = '0';
    group.style.height = '0';
    group.style.pointerEvents = 'none';
    group.style.zIndex = '50';
    effects.appendChild(group);

    const ring = document.createElement('div');
    Object.assign(ring.style, {
      position: 'absolute',
      left: '-9px',
      top: '-9px',
      width: '18px',
      height: '18px',
      border: '2px solid rgba(255,255,255,.96)',
      borderRadius: '999px',
      background: 'transparent',
      boxShadow: '0 0 0 1px rgba(0,0,0,.42), 0 0 14px rgba(255,255,255,.26)',
      transformOrigin: 'center center',
    });
    group.appendChild(ring);
    ring.animate(
      [
        { transform: 'translate(-50%, -50%) scale(.45)', opacity: .78 },
        { transform: 'translate(-50%, -50%) scale(2.8)', opacity: 0 },
      ],
      { duration: ms, easing: 'cubic-bezier(0, 0, 0.2, 1)', fill: 'forwards' }
    );

    for (let i = 0; i < 6; i += 1) {
      const a = (i / 6) * Math.PI * 2;
      const dx = Math.cos(a);
      const dy = Math.sin(a);
      const deg = a * 180 / Math.PI;
      const ray = document.createElement('div');
      Object.assign(ray.style, {
        position: 'absolute',
        width: '13px',
        height: '2px',
        borderRadius: '2px',
        background: 'rgba(255,255,255,.92)',
        boxShadow: '0 0 0 1px rgba(0,0,0,.38), 0 0 8px rgba(255,255,255,.28)',
        transformOrigin: 'center center',
      });
      group.appendChild(ray);
      ray.animate(
        [
          { transform: `translate(${dx * 12 - 8}px, ${dy * 12 - 1.5}px) rotate(${deg}deg) scaleX(.15)`, opacity: 0 },
          { transform: `translate(${dx * 22 - 8}px, ${dy * 22 - 1}px) rotate(${deg}deg) scaleX(.85)`, opacity: .85, offset: .3 },
          { transform: `translate(${dx * 36 - 8}px, ${dy * 36 - 1}px) rotate(${deg}deg) scaleX(.2)`, opacity: 0 },
        ],
        { duration: ms, delay: 25 + i * 14, easing: 'cubic-bezier(0, 0, 0.2, 1)', fill: 'forwards' }
      );
    }
    setTimeout(() => group.remove(), ms + 360);
  }

  async function click(x, y) {
    const contentPoint = { x: Number(x) || 0, y: Number(y) || 0 };
    if (state.cursor?.clickSync === 'block') await moveTo(contentPoint.x, contentPoint.y);
    else moveTo(contentPoint.x, contentPoint.y);
    const point = screenPoint(contentPoint);
    state.cursorPoint = point;
    burstAt(point, Math.max(260, Number(state.cursor?.clickMs) || 500));
  }

  function pill(text, position, opacity = 1) {
    const wrap = document.createElement('div');
    Object.assign(wrap.style, {
      position: 'absolute',
      left: '0',
      width: '100%',
      display: 'flex',
      justifyContent: 'center',
      padding: '0 24px',
      opacity: String(opacity),
      transition: 'opacity 180ms ease',
      boxSizing: 'border-box',
      zIndex: '70',
    });
    if (position === 'top') wrap.style.top = '32px';
    else if (position === 'center') {
      wrap.style.top = '50%';
      wrap.style.transform = 'translateY(-50%)';
    } else wrap.style.bottom = '72px';

    const el = document.createElement('div');
    el.textContent = text;
    Object.assign(el.style, {
      maxWidth: '80%',
      padding: '8px 20px',
      borderRadius: '8px',
      background: 'rgba(0,0,0,.75)',
      color: '#fff',
      font: '18px -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif',
      lineHeight: '1.45',
      textAlign: 'center',
      backdropFilter: 'blur(4px)',
      boxShadow: '0 8px 30px rgba(0,0,0,.22)',
    });
    wrap.appendChild(el);
    effects.appendChild(wrap);
    return wrap;
  }

  function overlayText(text, position = 'bottom', durationMs = 5000) {
    const previous = state.overlayChain;
    const generation = state.overlayGeneration;
    const ms = Math.max(1, Number(durationMs) || 5000);
    const shown = previous.then(() => {
      if (generation !== state.overlayGeneration) return null;
      effects.querySelectorAll('[data-agent-browser-recording-overlay]').forEach(el => el.remove());
      const el = pill(String(text || ''), position, 0);
      el.setAttribute('data-agent-browser-recording-overlay', '');
      requestAnimationFrame(() => { el.style.opacity = '1'; });
      return el;
    });
    state.overlayChain = shown.then(() => new Promise(resolve => {
      setTimeout(() => {
        if (generation === state.overlayGeneration) {
          effects.querySelectorAll('[data-agent-browser-recording-overlay]').forEach(el => {
            el.style.opacity = '0';
            setTimeout(() => el.remove(), 180);
          });
        }
        resolve();
      }, ms);
    })).catch(() => {});
    return shown.then(() => undefined);
  }

  function key(label) {
    const el = pill(String(label || ''), 'top', 0);
    el.setAttribute('data-agent-browser-recording-key', '');
    requestAnimationFrame(() => { el.style.opacity = '1'; });
    setTimeout(() => {
      el.style.opacity = '0';
      setTimeout(() => el.remove(), 180);
    }, 850);
  }

  function spotlight(x, y, durationMs = 1200, radius = null) {
    effects.querySelectorAll('[data-agent-browser-recording-spotlight]').forEach(el => el.remove());
    const point = screenPoint({ x: Number(x) || 0, y: Number(y) || 0 });
    const r = Math.max(36, Math.min(420, screenRadius(Number(radius) || 72)));
    const soft = Math.round(r * 1.7);
    const outer = Math.round(r * 3);
    const el = document.createElement('div');
    el.setAttribute('data-agent-browser-recording-spotlight', '');
    Object.assign(el.style, {
      position: 'absolute',
      inset: '0',
      background: `radial-gradient(circle at ${point.x}px ${point.y}px, rgba(118,255,56,.14) 0, rgba(118,255,56,.08) ${r}px, rgba(0,0,0,.16) ${soft}px, rgba(0,0,0,.50) ${outer}px)`,
      opacity: '0',
      transition: 'opacity 220ms ease',
      zIndex: '20',
    });
    const ring = document.createElement('div');
    Object.assign(ring.style, {
      position: 'absolute',
      left: `${point.x - r}px`,
      top: `${point.y - r}px`,
      width: `${r * 2}px`,
      height: `${r * 2}px`,
      border: '2px solid rgba(118,255,56,.92)',
      borderRadius: '999px',
      boxShadow: '0 0 0 1px rgba(0,0,0,.45), 0 0 26px rgba(118,255,56,.52), inset 0 0 18px rgba(118,255,56,.20)',
      transform: 'scale(.94)',
      opacity: '0',
    });
    el.appendChild(ring);
    effects.appendChild(el);
    requestAnimationFrame(() => {
      el.style.opacity = '1';
      ring.animate(
        [
          { transform: 'scale(.94)', opacity: 0 },
          { transform: 'scale(1)', opacity: 1 },
        ],
        { duration: 220, easing: 'cubic-bezier(0.4, 0, 0.2, 1)', fill: 'forwards' }
      );
    });
    setTimeout(() => {
      if (!el.isConnected) return;
      el.style.opacity = '0';
      setTimeout(() => el.remove(), 240);
    }, Math.max(1, Number(durationMs) || 1200));
  }

  function clearOverlay() {
    state.overlayGeneration += 1;
    state.overlayChain = Promise.resolve();
    effects.querySelectorAll('[data-agent-browser-recording-overlay], [data-agent-browser-recording-spotlight], [data-agent-browser-recording-key]').forEach(el => el.remove());
  }

  async function zoomTo(x, y, scale, durationMs = null) {
    const s = Math.max(1, Math.min(3, Number(scale) || 1));
    clearTimeout(state.zoomTimer);
    const generation = ++state.zoomGeneration;
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
    state.camera = { scale: s, originX, originY };
    camera.style.transformOrigin = `${originX}px ${originY}px`;
    camera.style.transition = 'transform 600ms cubic-bezier(0.4, 0, 0.2, 1)';
    camera.style.transform = `scale(${s})`;
    if (durationMs !== null && durationMs !== undefined) {
      state.zoomTimer = setTimeout(() => zoomReset(generation), Math.max(1, Number(durationMs) || 1));
    }
  }

  async function zoomReset(expectedGeneration = null) {
    clearTimeout(state.zoomTimer);
    const generation = expectedGeneration ?? ++state.zoomGeneration;
    if (expectedGeneration === null) state.zoomGeneration = generation;
    state.camera = { scale: 1, originX: state.camera.originX, originY: state.camera.originY };
    camera.style.transition = 'transform 600ms cubic-bezier(0.4, 0, 0.2, 1)';
    camera.style.transform = 'scale(1)';
    await new Promise(resolve => setTimeout(resolve, 700));
    if (generation !== state.zoomGeneration) return;
    camera.style.transformOrigin = '0 0';
    state.camera = { scale: 1, originX: 0, originY: 0 };
  }

  async function setFrame(jpegBase64) {
    const src = `data:image/jpeg;base64,${jpegBase64}`;
    if (frame.src === src) return;
    frame.src = src;
    if (frame.decode) {
      await frame.decode().catch(() => {});
    } else {
      await new Promise(resolve => {
        frame.onload = resolve;
        frame.onerror = resolve;
      });
    }
  }

  function cleanup() {
    clearTimeout(state.zoomTimer);
    effects.replaceChildren();
  }

  window.__agentBrowserRecordingEffects = {
    version: VERSION,
    configure(nextConfig = {}) {
      if (nextConfig.cursor === null) state.cursor = null;
      else state.cursor = { ...state.cursor, ...(nextConfig.cursor || {}) };
      ensureCursor();
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
  window.__agentBrowserRecordingCompositor = { version: VERSION, setFrame };
  ensureCursor();
})();
"#;
