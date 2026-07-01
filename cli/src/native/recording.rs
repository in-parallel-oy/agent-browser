use serde_json::{json, Value};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::sync::{oneshot, Mutex, Notify, RwLock};

use super::cdp::client::CdpClient;
use super::cdp::types::{CaptureScreenshotParams, CaptureScreenshotResult};

/// Default capture cadence. 30 fps is the lowest rate at which sub-frame
/// motion (cursor tween, click ripple) reads as smooth animation; 10 fps
/// strobes too much to register tween motion at all when the page is
/// otherwise still. ffmpeg pipe cost is negligible at these rates.
pub const DEFAULT_CAPTURE_FPS: u32 = 30;
/// Lower bound. Below this, ffmpeg's image2pipe demuxer rejects timestamps.
const MIN_CAPTURE_FPS: u32 = 1;
/// Upper bound. Anything higher and CDP captureScreenshot can't keep up.
const MAX_CAPTURE_FPS: u32 = 60;
const MAX_CATCH_UP_SECONDS: u64 = 2;

pub struct RecordingState {
    pub active: bool,
    pub output_path: String,
    pub frame_count: u64,
    pub fps: u32,
    pub capture_task: Option<tokio::task::JoinHandle<Result<(), String>>>,
    pub shared_frame_count: Option<Arc<AtomicU64>>,
    pub cancel_tx: Option<oneshot::Sender<()>>,
    pub stop_post_roll: Duration,
    pub capture_gate: Option<Arc<RecordingCaptureGate>>,
    pub capture_session: Option<Arc<RwLock<String>>>,
}

impl RecordingState {
    pub fn new() -> Self {
        Self {
            active: false,
            output_path: String::new(),
            frame_count: 0,
            fps: DEFAULT_CAPTURE_FPS,
            capture_task: None,
            shared_frame_count: None,
            cancel_tx: None,
            stop_post_roll: Duration::ZERO,
            capture_gate: None,
            capture_session: None,
        }
    }
}

/// Activity gate for demo recordings. Normal recordings capture wall-clock
/// time continuously; demo recordings only capture while a browser action or
/// recording effect is visually active, so agent inference time is not encoded
/// into the video.
#[derive(Debug)]
pub struct RecordingCaptureGate {
    active_until: Mutex<Option<Instant>>,
    notify: Notify,
}

impl RecordingCaptureGate {
    pub fn new_paused() -> Self {
        Self {
            active_until: Mutex::new(None),
            notify: Notify::new(),
        }
    }

    pub async fn activate_for(&self, duration: Duration) {
        if duration.is_zero() {
            return;
        }
        let until = Instant::now() + duration;
        let mut active_until = self.active_until.lock().await;
        if active_until.is_none_or(|current| until > current) {
            *active_until = Some(until);
        }
        drop(active_until);
        self.notify.notify_one();
    }

    pub(crate) async fn is_active(&self) -> bool {
        self.active_until
            .lock()
            .await
            .is_some_and(|until| until > Instant::now())
    }

    pub(crate) async fn wait_until_active(&self) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_active().await {
                return;
            }
            notified.await;
        }
    }
}

/// Clamp a user-supplied fps value (from `--record-fps`) into the supported
/// range. Returns `Err` with a descriptive message on out-of-range input so
/// the daemon can surface it before starting the capture task.
pub fn validate_fps(fps: u32) -> Result<u32, String> {
    if !(MIN_CAPTURE_FPS..=MAX_CAPTURE_FPS).contains(&fps) {
        return Err(format!(
            "fps must be between {} and {} (got {})",
            MIN_CAPTURE_FPS, MAX_CAPTURE_FPS, fps
        ));
    }
    Ok(fps)
}

pub fn recording_start(state: &mut RecordingState, path: &str) -> Result<Value, String> {
    if state.active {
        return Err("Recording already active".to_string());
    }

    state.active = true;
    state.output_path = path.to_string();
    state.frame_count = 0;
    state.capture_gate = None;
    state.capture_session = None;

    Ok(json!({ "started": true, "path": path }))
}

pub fn recording_stop(state: &mut RecordingState) -> Result<Value, String> {
    if !state.active {
        return Err("No recording in progress".to_string());
    }

    state.active = false;
    state.capture_gate = None;
    state.capture_session = None;

    if state.frame_count == 0 {
        return Err("No frames captured".to_string());
    }

    Ok(json!({ "path": &state.output_path, "frames": state.frame_count }))
}

pub fn recording_abort(state: &mut RecordingState) -> Result<Value, String> {
    if !state.active {
        return Err("No recording in progress".to_string());
    }

    let path = state.output_path.clone();
    state.active = false;
    state.output_path.clear();
    state.frame_count = 0;
    state.capture_gate = None;
    state.capture_session = None;

    Ok(json!({ "aborted": true, "path": path }))
}

pub fn recording_restart(state: &mut RecordingState, path: &str) -> Result<Value, String> {
    let previous = if state.active {
        let stop_result = recording_stop(state);
        stop_result
            .ok()
            .and_then(|v| v.get("path").and_then(|p| p.as_str()).map(String::from))
    } else {
        None
    };

    recording_start(state, path)?;

    Ok(json!({
        "restarted": true,
        "previousPath": previous,
        "path": path,
    }))
}

pub(crate) fn build_ffmpeg_command(output_path: &str, fps: u32) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("ffmpeg");

    cmd.args(["-y"])
        .args(["-avioflags", "direct"])
        .args([
            "-fpsprobesize",
            "0",
            "-probesize",
            "32",
            "-analyzeduration",
            "0",
        ])
        .args([
            "-f",
            "image2pipe",
            "-c:v",
            "mjpeg",
            "-framerate",
            &fps.to_string(),
            "-i",
            "pipe:0",
        ])
        .args(["-vf", "pad=ceil(iw/2)*2:ceil(ih/2)*2"]);

    if output_path.ends_with(".webm") {
        cmd.args(["-c:v", "libvpx", "-crf", "30", "-b:v", "1M"]);
    } else {
        cmd.args(["-c:v", "libx264", "-preset", "ultrafast"]);
    }

    cmd.args(["-pix_fmt", "yuv420p", "-threads", "1"])
        .arg(output_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    cmd
}

pub(crate) fn capture_interval(fps: u32) -> Duration {
    // Round down because if fps doesn't divide evenly, we'd rather capture
    // slightly faster than the declared rate.
    let fps = fps.max(1) as u64;
    Duration::from_millis(1000 / fps)
}

pub(crate) fn due_frame_count(
    started_at: Instant,
    now: Instant,
    fps: u32,
    frames_written: u64,
) -> u64 {
    let elapsed = now
        .checked_duration_since(started_at)
        .unwrap_or(Duration::ZERO);
    let target_frames = (elapsed.as_secs_f64() * fps.max(1) as f64).floor() as u64 + 1;
    let max_catch_up_frames = fps.max(1) as u64 * MAX_CATCH_UP_SECONDS;
    target_frames
        .saturating_sub(frames_written)
        .max(1)
        .min(max_catch_up_frames)
}

/// Spawn a background task that captures screenshots at a fixed interval
/// and pipes them to ffmpeg in real-time.
pub fn spawn_recording_task(
    client: Arc<CdpClient>,
    session_id: Arc<RwLock<String>>,
    output_path: String,
    fps: u32,
    shared_count: Arc<AtomicU64>,
    cancel_rx: oneshot::Receiver<()>,
    capture_gate: Option<Arc<RecordingCaptureGate>>,
) -> tokio::task::JoinHandle<Result<(), String>> {
    tokio::spawn(async move {
        let mut cancel_rx = std::pin::pin!(cancel_rx);

        let mut ffmpeg = build_ffmpeg_command(&output_path, fps)
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

        let mut interval = tokio::time::interval(capture_interval(fps));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut started_at = None;
        let mut segment_frames_written = 0_u64;

        let params = CaptureScreenshotParams {
            format: Some("jpeg".to_string()),
            quality: Some(80),
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

            let current_session_id = session_id.read().await.clone();
            let result: Result<CaptureScreenshotResult, _> = client
                .send_command_typed("Page.captureScreenshot", &params, Some(&current_session_id))
                .await;

            let screenshot = match result {
                Ok(s) => s,
                Err(e) => {
                    if e.contains("Target closed") || e.contains("not found") {
                        break;
                    }
                    continue;
                }
            };

            let bytes = match base64::Engine::decode(
                &base64::engine::general_purpose::STANDARD,
                &screenshot.data,
            ) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let now = Instant::now();
            let capture_started_at = *started_at.get_or_insert(now);
            let due_frames = due_frame_count(capture_started_at, now, fps, segment_frames_written);
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

        let output = ffmpeg
            .wait_with_output()
            .await
            .map_err(|e| format!("ffmpeg wait failed: {}", e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "ffmpeg failed: {}",
                stderr.chars().take(300).collect::<String>()
            ));
        }

        Ok(())
    })
}

pub async fn stop_recording_task(state: &mut RecordingState) -> Result<(), String> {
    if let Some(tx) = state.cancel_tx.take() {
        let _ = tx.send(());
    }

    let counter = state.shared_frame_count.take();
    let handle = state.capture_task.take();

    let result = if let Some(h) = handle {
        match h.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(e),
            Err(e) => Err(format!("Recording task panicked: {}", e)),
        }
    } else {
        Ok(())
    };

    if let Some(c) = counter {
        state.frame_count = c.load(Ordering::Relaxed);
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_recording_state_new() {
        let state = RecordingState::new();
        assert!(!state.active);
        assert!(state.output_path.is_empty());
        assert_eq!(state.frame_count, 0);
    }

    #[test]
    fn test_recording_start_sets_active() {
        let mut state = RecordingState::new();
        let result = recording_start(&mut state, "/tmp/test.mp4");
        assert!(result.is_ok());
        assert!(state.active);
        assert_eq!(state.output_path, "/tmp/test.mp4");
        assert_eq!(state.frame_count, 0);
    }

    #[test]
    fn test_recording_start_while_active() {
        let mut state = RecordingState::new();
        recording_start(&mut state, "/tmp/test1.mp4").unwrap();
        let result = recording_start(&mut state, "/tmp/test2.mp4");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("already active"));
    }

    #[test]
    fn test_recording_stop_not_active() {
        let mut state = RecordingState::new();
        let result = recording_stop(&mut state);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("No recording"));
    }

    #[test]
    fn test_recording_stop_no_frames() {
        let mut state = RecordingState::new();
        recording_start(&mut state, "/tmp/test.mp4").unwrap();
        let result = recording_stop(&mut state);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("No frames"));
        assert!(!state.active);
    }

    #[test]
    fn test_recording_abort_discards_state_without_frames() {
        let mut state = RecordingState::new();
        recording_start(&mut state, "/tmp/test.webm").unwrap();
        let result = recording_abort(&mut state).unwrap();

        assert_eq!(result["aborted"], true);
        assert_eq!(result["path"], "/tmp/test.webm");
        assert!(!state.active);
        assert_eq!(state.frame_count, 0);
        assert!(state.output_path.is_empty());
    }

    #[test]
    fn test_recording_restart_while_inactive() {
        let mut state = RecordingState::new();
        let result = recording_restart(&mut state, "/tmp/new.webm");
        assert!(result.is_ok());
        assert!(state.active);
        assert_eq!(state.output_path, "/tmp/new.webm");
    }

    #[test]
    fn test_recording_restart_while_active() {
        let mut state = RecordingState::new();
        recording_start(&mut state, "/tmp/old.webm").unwrap();
        state.frame_count = 10;
        let result = recording_restart(&mut state, "/tmp/new.webm").unwrap();
        assert!(state.active);
        assert_eq!(state.output_path, "/tmp/new.webm");
        assert_eq!(state.frame_count, 0);
        assert_eq!(result["previousPath"], "/tmp/old.webm");
    }

    #[test]
    fn test_build_ffmpeg_command_webm() {
        let cmd = build_ffmpeg_command("/tmp/out.webm", DEFAULT_CAPTURE_FPS);
        let args: Vec<&std::ffi::OsStr> = cmd.as_std().get_args().collect();
        let args_str: Vec<&str> = args.iter().filter_map(|a| a.to_str()).collect();
        assert!(args_str.contains(&"libvpx"));
        assert!(args_str.contains(&"/tmp/out.webm"));
    }

    #[test]
    fn test_build_ffmpeg_command_mp4() {
        let cmd = build_ffmpeg_command("/tmp/out.mp4", DEFAULT_CAPTURE_FPS);
        let args: Vec<&std::ffi::OsStr> = cmd.as_std().get_args().collect();
        let args_str: Vec<&str> = args.iter().filter_map(|a| a.to_str()).collect();
        assert!(args_str.contains(&"libx264"));
        assert!(args_str.contains(&"/tmp/out.mp4"));
    }

    #[test]
    fn test_build_ffmpeg_command_passes_fps_to_framerate() {
        let cmd = build_ffmpeg_command("/tmp/out.webm", 24);
        let args: Vec<&std::ffi::OsStr> = cmd.as_std().get_args().collect();
        let args_str: Vec<&str> = args.iter().filter_map(|a| a.to_str()).collect();
        let fr_idx = args_str
            .iter()
            .position(|a| *a == "-framerate")
            .expect("ffmpeg invocation should include -framerate");
        assert_eq!(args_str[fr_idx + 1], "24");
    }

    #[test]
    fn test_capture_interval_for_fps() {
        assert_eq!(capture_interval(30).as_millis(), 33);
        assert_eq!(capture_interval(10).as_millis(), 100);
        assert_eq!(capture_interval(60).as_millis(), 16);
    }

    #[test]
    fn test_due_frame_count_preserves_wall_clock_time() {
        let started_at = Instant::now();
        assert_eq!(due_frame_count(started_at, started_at, 30, 0), 1);
        assert_eq!(
            due_frame_count(started_at, started_at + Duration::from_millis(500), 30, 1),
            15
        );
        assert_eq!(
            due_frame_count(started_at, started_at + Duration::from_millis(1000), 30, 16),
            15
        );
    }

    #[test]
    fn test_due_frame_count_caps_long_stall_catch_up() {
        let started_at = Instant::now();
        assert_eq!(
            due_frame_count(started_at, started_at + Duration::from_secs(10), 30, 1),
            60
        );
    }

    #[tokio::test]
    async fn test_recording_capture_gate_starts_paused() {
        let gate = RecordingCaptureGate::new_paused();
        assert!(!gate.is_active().await);

        gate.activate_for(Duration::from_millis(25)).await;
        assert!(gate.is_active().await);
    }

    #[test]
    fn test_validate_fps_bounds() {
        assert!(validate_fps(0).is_err());
        assert!(validate_fps(1).is_ok());
        assert!(validate_fps(30).is_ok());
        assert!(validate_fps(60).is_ok());
        assert!(validate_fps(61).is_err());
    }
}
