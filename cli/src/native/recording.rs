use serde_json::{json, Value};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::oneshot;

use super::cdp::client::CdpClient;
use super::cdp::types::{CaptureScreenshotParams, CaptureScreenshotResult};

/// Default capture cadence. 30 fps is the lowest rate at which sub-frame
/// motion (cursor tween, click ripple) reads as smooth animation; 10 fps
/// strobes too much to register tween motion at all when the page is
/// otherwise still. ffmpeg pipe cost is negligible at these rates.
pub const DEFAULT_CAPTURE_FPS: u32 = 30;
/// Lower bound -- below this, ffmpeg's image2pipe demuxer rejects timestamps.
const MIN_CAPTURE_FPS: u32 = 1;
/// Upper bound. Anything higher and CDP captureScreenshot can't keep up.
const MAX_CAPTURE_FPS: u32 = 60;

pub struct RecordingState {
    pub active: bool,
    pub output_path: String,
    pub frame_count: u64,
    pub fps: u32,
    pub capture_task: Option<tokio::task::JoinHandle<Result<(), String>>>,
    pub shared_frame_count: Option<Arc<AtomicU64>>,
    pub cancel_tx: Option<oneshot::Sender<()>>,
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

    Ok(json!({ "started": true, "path": path }))
}

pub fn recording_stop(state: &mut RecordingState) -> Result<Value, String> {
    if !state.active {
        return Err("No recording in progress".to_string());
    }

    state.active = false;

    if state.frame_count == 0 {
        return Err("No frames captured".to_string());
    }

    Ok(json!({ "path": &state.output_path, "frames": state.frame_count }))
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

fn build_ffmpeg_command(output_path: &str, fps: u32) -> tokio::process::Command {
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

fn capture_interval(fps: u32) -> Duration {
    // Round down -- if fps doesn't divide evenly, we'd rather capture slightly
    // faster than the declared rate (ffmpeg drops/dupes to the encoder framerate).
    let fps = fps.max(1) as u64;
    Duration::from_millis(1000 / fps)
}

/// Spawn a background task that captures screenshots at a fixed interval
/// and pipes them to ffmpeg in real-time.
pub fn spawn_recording_task(
    client: Arc<CdpClient>,
    session_id: String,
    output_path: String,
    fps: u32,
    shared_count: Arc<AtomicU64>,
    cancel_rx: oneshot::Receiver<()>,
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

        let params = CaptureScreenshotParams {
            format: Some("jpeg".to_string()),
            quality: Some(80),
            clip: None,
            from_surface: Some(true),
            capture_beyond_viewport: None,
        };

        loop {
            tokio::select! {
                _ = &mut cancel_rx => break,
                _ = interval.tick() => {}
            }

            let result: Result<CaptureScreenshotResult, _> = client
                .send_command_typed("Page.captureScreenshot", &params, Some(&session_id))
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

            if stdin.write_all(&bytes).await.is_err() {
                break;
            }
            shared_count.fetch_add(1, Ordering::Relaxed);
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
    fn test_validate_fps_bounds() {
        assert!(validate_fps(0).is_err());
        assert!(validate_fps(1).is_ok());
        assert!(validate_fps(30).is_ok());
        assert!(validate_fps(60).is_ok());
        assert!(validate_fps(61).is_err());
    }
}
