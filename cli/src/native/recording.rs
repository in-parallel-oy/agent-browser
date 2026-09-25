use serde_json::{json, Value};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot, OwnedSemaphorePermit, Semaphore};

use super::cdp::client::CdpClient;
use super::cdp::types::{AttachToTargetParams, AttachToTargetResult};

/// Capture rate used when the caller does not ask for one. 30 fps reads as
/// smooth motion, so scrolls, hovers, and CSS transitions survive the
/// recording instead of turning into a slideshow.
pub const DEFAULT_FPS: u32 = 30;

/// Highest capture rate the recorder accepts. 60 fps is worth asking for on
/// short, motion-heavy clips (drag interactions, animation, scroll polish
/// work) where the extra temporal detail is the point.
pub const MAX_FPS: u32 = 60;

/// Rate above which the encoder switches to its high-frame-rate profile:
/// twice the bitrate budget and a second encoder thread, so the encoder keeps
/// pace with capture instead of filling its queue.
const HIGH_FPS_THRESHOLD: u32 = 30;

/// Bitrate budget for WebM at [`HIGH_FPS_THRESHOLD`], scaled linearly with
/// the requested rate. VP8 at 60 fps needs roughly twice the bits to hold the
/// same per-frame quality.
const WEBM_BITRATE_KBPS_AT_BASE_FPS: u32 = 1000;

/// Longest gap the recorder fills with held frames, in seconds. A page that
/// produces no frames for longer than this (a hang, or a tab left in the
/// background) is held for this long and the remainder is dropped from the
/// timeline, so a stalled page cannot inflate the file.
const MAX_BACKFILL_SECS: u64 = 5;

/// Screencast frames buffered ahead of the ticker. Two absorbs the jitter
/// between Chrome's frame clock and the recorder's without letting a lower
/// recording rate fall behind the page.
const MAX_PENDING_FRAMES: usize = 2;

/// Seconds of video the encoder may fall behind capture. Chrome sends no new
/// screencast frame until the previous ones are acknowledged, so capture
/// acknowledges and times frames as they arrive and queues them for ffmpeg:
/// an encoder briefly starved of CPU then delays the file instead of the
/// screencast. A full queue makes capture wait for the encoder again, which
/// bounds the queue's memory and how long `record stop` spends draining it.
const MAX_ENCODER_BACKLOG_SECS: u64 = 5;

/// Longest `record stop` waits for the encoder to drain its queue: a full
/// backlog at a third of real-time speed, inside the CLI's 30 s response
/// budget. Past it ffmpeg is taken to have stopped reading and is killed.
const ENCODER_DRAIN_LIMIT: Duration = Duration::from_secs(3 * MAX_ENCODER_BACKLOG_SECS);

/// JPEG quality requested from `Page.startScreencast`. Matches the quality the
/// recorder used to request from `Page.captureScreenshot`.
const SCREENCAST_QUALITY: u32 = 80;

/// Upper bound on waiting for Chrome to acknowledge screencast teardown.
/// The page may already be gone by the time a recording stops.
const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(2);

const AUDIO_SAMPLE_RATE: u32 = 48_000;
const AUDIO_CHANNELS: u16 = 2;
const EMBEDDED_WAV_HEADER_LEN: usize = 44;
const KEYBOARD_GAIN: f32 = 0.18;
const KEYBOARD_CROSSFADE_SAMPLES: u64 = AUDIO_SAMPLE_RATE as u64 * 20 / 1_000;
const KEYBOARD_EDGE_FADE_SAMPLES: u64 = AUDIO_SAMPLE_RATE as u64 * 10 / 1_000;

static CLICK_WAV: &[u8] = include_bytes!("../../assets/recording/click.wav");
static KEYBOARD_WAV: &[u8] = include_bytes!("../../assets/recording/keyboard.wav");
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A frame queued for the encoder and the number of consecutive slots it
/// fills.
type SlotRun = (Arc<[u8]>, u64);

/// A [`SlotRun`] with its share of the encoder backlog, returned once written.
type QueuedRun = (SlotRun, OwnedSemaphorePermit);

#[derive(Clone, Copy, Debug)]
enum RecordedSound {
    Click,
    Keyboard,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SoundEvent {
    Click { frame: u64 },
    Keyboard { start_frame: u64, end_frame: u64 },
}

#[derive(Clone, Debug)]
pub struct RecordingSoundHandle {
    events: Arc<Mutex<Vec<SoundEvent>>>,
    frame_count: Arc<AtomicU64>,
    fps: u32,
}

impl RecordingSoundHandle {
    pub fn click(&self) {
        let frame = self.frame_count.load(Ordering::Relaxed);
        if let Ok(mut events) = self.events.lock() {
            events.push(SoundEvent::Click { frame });
        }
    }

    pub fn keyboard_ending_now(&self, duration: Duration) {
        if duration.is_zero() {
            return;
        }
        let end_frame = self.frame_count.load(Ordering::Relaxed);
        let duration_frames = duration
            .as_millis()
            .saturating_mul(u128::from(self.fps))
            .div_ceil(1_000) as u64;
        let start_frame = end_frame.saturating_sub(duration_frames.max(1));
        if end_frame <= start_frame {
            return;
        }
        if let Ok(mut events) = self.events.lock() {
            append_keyboard_sound(&mut events, start_frame, end_frame);
        }
    }
}

/// Characters of ffmpeg's stderr kept in a failure message. ffmpeg prints
/// the cause last, after the banner and stream summaries, so the tail is
/// the part worth showing.
const FFMPEG_STDERR_TAIL_CHARS: usize = 400;

/// Lower-cased extension of `path`, if it has one.
fn output_extension(path: &str) -> Option<String> {
    std::path::Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
}

/// Reject output paths with no extension. ffmpeg picks the container from
/// the extension, so a bare name fails at `record stop` with the take
/// already lost. Anything with an extension is handed to ffmpeg as-is:
/// `.webm` gets libvpx, everything else libx264 in whatever container
/// ffmpeg knows for that extension.
pub fn validate_output_path(path: &str) -> Result<(), String> {
    match output_extension(path) {
        Some(ext) if !ext.is_empty() => Ok(()),
        _ => Err(format!(
            "Invalid output path: '{}' has no extension (use .webm or .mp4)",
            path
        )),
    }
}

/// The last [`FFMPEG_STDERR_TAIL_CHARS`] of `stderr`, cut at a line boundary
/// where one falls inside the window, with trailing whitespace removed.
fn ffmpeg_error_tail(stderr: &str) -> String {
    let trimmed = stderr.trim_end();
    let total = trimmed.chars().count();
    if total <= FFMPEG_STDERR_TAIL_CHARS {
        return trimmed.to_string();
    }
    let tail: String = trimmed
        .chars()
        .skip(total - FFMPEG_STDERR_TAIL_CHARS)
        .collect();
    // Drop the partial first line so the message starts on a whole one.
    match tail.find(['\n', '\r']) {
        Some(cut) if cut + 1 < tail.len() => tail[cut + 1..].trim_start().to_string(),
        _ => tail,
    }
}

/// Reject frame rates the pipeline cannot honor.
pub fn validate_fps(fps: u32) -> Result<u32, String> {
    if fps == 0 || fps > MAX_FPS {
        return Err(format!(
            "Invalid fps: {} is out of range (valid range: 1-{})",
            fps, MAX_FPS
        ));
    }
    Ok(fps)
}

/// Wall-clock duration of one frame at `fps`.
fn frame_period(fps: u32) -> Duration {
    Duration::from_micros(1_000_000 / fps.clamp(1, MAX_FPS) as u64)
}

/// Frames owed to the constant-rate stream at `elapsed` into the recording,
/// given `written` frames already sent. Zero when the current slot is filled.
fn frames_due(elapsed: Duration, period: Duration, written: u64) -> u64 {
    let period_us = period.as_micros().max(1);
    let slot = (elapsed.as_micros() / period_us) as u64;
    slot.saturating_add(1).saturating_sub(written)
}

/// Encoder queue length at `fps`, in slots: [`MAX_ENCODER_BACKLOG_SECS`] of
/// video.
fn encoder_backlog_slots(fps: u32) -> u32 {
    MAX_ENCODER_BACKLOG_SECS as u32 * fps.clamp(1, MAX_FPS)
}

/// Capture's end of the encoder queue. The bound is in slots, not entries:
/// a frame held through a gap is one entry, but ffmpeg encodes it once per
/// slot, so an entry bound would let a slow encoder fall minutes behind.
struct EncoderQueue {
    runs: mpsc::UnboundedSender<QueuedRun>,
    slots: Arc<Semaphore>,
    capacity: u32,
}

impl EncoderQueue {
    fn new(fps: u32) -> (Self, mpsc::UnboundedReceiver<QueuedRun>) {
        let capacity = encoder_backlog_slots(fps);
        let (runs, rx) = mpsc::unbounded_channel();
        let queue = Self {
            runs,
            slots: Arc::new(Semaphore::new(capacity as usize)),
            capacity,
        };
        (queue, rx)
    }

    /// Queue `run` once the backlog has room for it. A run longer than the
    /// whole backlog waits for an empty queue. Fails once the writer is gone.
    async fn send(&self, run: SlotRun) -> Result<(), ()> {
        let wanted = run.1.min(self.capacity as u64) as u32;
        let permit = Arc::clone(&self.slots)
            .acquire_many_owned(wanted)
            .await
            .map_err(|_| ())?;
        self.runs.send((run, permit)).map_err(|_| ())
    }

    /// Slots queued or being written.
    fn queued_slots(&self) -> u32 {
        self.capacity - self.slots.available_permits() as u32
    }
}

/// Split the `emit` slots owed on one tick into encoder queue entries.
/// Pending frames go out in arrival order, one slot each, and the last of
/// them (or the previous frame, when none arrived) holds the remaining
/// slots as a single entry however long the gap.
fn fill_slots(
    pending: &mut VecDeque<Arc<[u8]>>,
    last: &mut Option<Arc<[u8]>>,
    emit: u64,
) -> Vec<SlotRun> {
    let mut runs = Vec::new();
    let mut used = 0;
    while used < emit {
        if let Some(next) = pending.pop_front() {
            *last = Some(next);
        }
        let Some(frame) = last.clone() else { break };
        let slots = if pending.is_empty() { emit - used } else { 1 };
        runs.push((frame, slots));
        used += slots;
    }
    runs
}

/// The CDP session a recording attaches to its page target for its screencast.
///
/// Chrome keeps one screencast per session and the live stream already runs
/// one on the page session, so the recorder attaches a second flattened
/// session to the same target. The daemon's event handlers use this to avoid
/// treating that attachment as a tab.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureSession {
    pub target_id: String,
    /// `None` while `Target.attachToTarget` is in flight: Chrome emits
    /// `Target.attachedToTarget` before it answers, so in that window the
    /// attachment is recognised by target instead.
    pub session_id: Option<String>,
}

impl CaptureSession {
    /// Whether a `Target.attachedToTarget` event is this recorder's own
    /// attachment rather than a tab the daemon should track.
    pub fn owns_attachment(&self, target_id: &str, session_id: &str) -> bool {
        match self.session_id.as_deref() {
            Some(own) => own == session_id,
            None => self.target_id == target_id,
        }
    }
}

pub type SharedCaptureSession = Arc<Mutex<Option<CaptureSession>>>;

pub struct RecordingState {
    pub active: bool,
    pub output_path: String,
    /// Capture rate for the active (or most recent) recording.
    pub fps: u32,
    /// Frames written to the file, including frames held through gaps.
    pub frame_count: u64,
    /// Distinct frames received from the screencast.
    pub captured_count: u64,
    pub capture_task: Option<tokio::task::JoinHandle<Result<(), String>>>,
    pub shared_frame_count: Option<Arc<AtomicU64>>,
    pub shared_captured_count: Option<Arc<AtomicU64>>,
    /// Ends capture; `true` discards the take instead of finishing the file.
    pub cancel_tx: Option<oneshot::Sender<bool>>,
    /// Extra capture time requested when stopping a demo recording.
    pub stop_post_roll: Duration,
    /// Activity gate used by demo mode to omit time between visual actions.
    pub capture_gate: Option<Arc<RecordingCaptureGate>>,
    sound_events: Arc<Mutex<Vec<SoundEvent>>>,
    /// Shared with the daemon's event handlers.
    pub capture_session: SharedCaptureSession,
}

impl RecordingState {
    pub fn new() -> Self {
        Self {
            active: false,
            output_path: String::new(),
            fps: DEFAULT_FPS,
            frame_count: 0,
            captured_count: 0,
            capture_task: None,
            shared_frame_count: None,
            shared_captured_count: None,
            cancel_tx: None,
            stop_post_roll: Duration::ZERO,
            capture_gate: None,
            sound_events: Arc::new(Mutex::new(Vec::new())),
            capture_session: Arc::new(Mutex::new(None)),
        }
    }
}

/// Activity gate for compact demo recordings. Chromium continues producing
/// and receiving screencast frames while paused, but the encoder advances only
/// while a visual action or recording effect is active.
#[derive(Debug)]
pub struct RecordingCaptureGate {
    state: Mutex<RecordingCaptureGateState>,
}

#[derive(Debug, Default)]
struct RecordingCaptureGateState {
    active_actions: u32,
    active_until: Option<tokio::time::Instant>,
}

impl RecordingCaptureGate {
    pub fn new_paused() -> Self {
        Self {
            state: Mutex::new(RecordingCaptureGateState::default()),
        }
    }

    pub async fn activate_for(&self, duration: Duration) {
        if duration.is_zero() {
            return;
        }
        let until = tokio::time::Instant::now() + duration;
        if let Ok(mut state) = self.state.lock() {
            if state.active_until.is_none_or(|current| until > current) {
                state.active_until = Some(until);
            }
        }
    }

    /// Keep capture open for the complete lifetime of a visual command. This
    /// avoids estimating an action's duration before browser-side scrolling,
    /// navigation, or animation has actually completed.
    pub async fn begin_action(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.active_actions = state.active_actions.saturating_add(1);
        }
    }

    /// Finish one visual command and retain a short tail for its final paint.
    pub async fn end_action(&self, tail: Duration) {
        if let Ok(mut state) = self.state.lock() {
            state.active_actions = state.active_actions.saturating_sub(1);
            if !tail.is_zero() {
                let until = tokio::time::Instant::now() + tail;
                if state.active_until.is_none_or(|current| until > current) {
                    state.active_until = Some(until);
                }
            }
        }
    }

    fn is_active(&self) -> bool {
        self.state.lock().ok().is_some_and(|state| {
            state.active_actions > 0
                || state
                    .active_until
                    .is_some_and(|until| until > tokio::time::Instant::now())
        })
    }
}

impl RecordingState {
    pub fn sound_handle(&self) -> Option<RecordingSoundHandle> {
        if !self.active {
            return None;
        }
        Some(RecordingSoundHandle {
            events: Arc::clone(&self.sound_events),
            frame_count: Arc::clone(self.shared_frame_count.as_ref()?),
            fps: self.fps,
        })
    }
}

/// [`CaptureSession::owns_attachment`] against the shared slot.
pub fn owns_attachment(shared: &SharedCaptureSession, target_id: &str, session_id: &str) -> bool {
    shared
        .lock()
        .ok()
        .and_then(|guard| {
            guard
                .as_ref()
                .map(|c| c.owns_attachment(target_id, session_id))
        })
        .unwrap_or(false)
}

pub fn recording_start(
    state: &mut RecordingState,
    path: &str,
    fps: Option<u32>,
) -> Result<Value, String> {
    if state.active {
        return Err("Recording already active".to_string());
    }

    validate_output_path(path)?;
    let fps = validate_fps(fps.unwrap_or(DEFAULT_FPS))?;

    state.active = true;
    state.output_path = path.to_string();
    state.fps = fps;
    state.frame_count = 0;
    state.captured_count = 0;
    state.stop_post_roll = Duration::ZERO;
    state.capture_gate = None;
    state.shared_frame_count = Some(Arc::new(AtomicU64::new(0)));
    if let Ok(mut events) = state.sound_events.lock() {
        events.clear();
    }

    Ok(json!({ "started": true, "path": path, "fps": fps }))
}

pub fn recording_stop(state: &mut RecordingState) -> Result<Value, String> {
    if !state.active {
        return Err("No recording in progress".to_string());
    }

    state.active = false;
    state.capture_gate = None;

    if state.frame_count == 0 {
        return Err("No frames captured".to_string());
    }

    Ok(json!({
        "path": &state.output_path,
        "frames": state.frame_count,
        "capturedFrames": state.captured_count,
        "fps": state.fps,
    }))
}

pub fn recording_abort(state: &mut RecordingState) -> Result<Value, String> {
    if !state.active {
        return Err("No recording in progress".to_string());
    }

    let path = state.output_path.clone();
    state.active = false;
    state.output_path.clear();
    state.frame_count = 0;
    state.captured_count = 0;
    state.stop_post_roll = Duration::ZERO;
    state.capture_gate = None;

    Ok(json!({ "aborted": true, "path": path }))
}

fn build_ffmpeg_command(output_path: &str, fps: u32) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("ffmpeg");
    let high_fps = fps > HIGH_FPS_THRESHOLD;

    // -hide_banner keeps the version and build banner out of stderr, so a
    // failure message is the cause rather than the configure line.
    // -nostats because nothing reads stderr until ffmpeg exits: two progress
    // lines a second fill the pipe within minutes and wedge the encoder.
    cmd.args(["-y", "-hide_banner", "-nostats"])
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

    if output_extension(output_path).as_deref() == Some("webm") {
        let bitrate = WEBM_BITRATE_KBPS_AT_BASE_FPS
            .max(WEBM_BITRATE_KBPS_AT_BASE_FPS.saturating_mul(fps) / HIGH_FPS_THRESHOLD.max(1));
        // The realtime deadline lets libvpx pick its speed per frame to fit
        // a time budget, half the frame period at -cpu-used 8, so a loaded
        // machine gets a softer picture rather than a backlog. The default
        // "good" deadline has no budget and is several times slower.
        cmd.args(["-c:v", "libvpx"])
            .args(["-deadline", "realtime", "-cpu-used", "8"])
            .args(["-crf", "30"])
            .args(["-b:v", &format!("{}k", bitrate)]);
    } else {
        cmd.args(["-c:v", "libx264", "-preset", "ultrafast"]);
    }

    // One encoder thread keeps CPU away from the browser at ordinary rates;
    // above 30 fps the encoder needs a second one to drain the pipe in time.
    cmd.args(["-pix_fmt", "yuv420p"])
        .args(["-threads", if high_fps { "2" } else { "1" }])
        .arg(output_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    cmd
}

/// Attach the recorder's own flattened session to the target behind
/// `page_session_id`, publishing it in `shared` before the command is sent so
/// the resulting `Target.attachedToTarget` is recognised as the recorder's.
pub async fn attach_capture_session(
    client: &CdpClient,
    page_session_id: &str,
    shared: &SharedCaptureSession,
) -> Result<String, String> {
    let info = client
        .send_command_no_params("Target.getTargetInfo", Some(page_session_id))
        .await?;
    let target_id = info
        .get("targetInfo")
        .and_then(|t| t.get("targetId"))
        .and_then(Value::as_str)
        .ok_or("Failed to resolve recording target")?
        .to_string();

    if let Ok(mut guard) = shared.lock() {
        *guard = Some(CaptureSession {
            target_id: target_id.clone(),
            session_id: None,
        });
    }

    let attached: Result<AttachToTargetResult, String> = client
        .send_command_typed(
            "Target.attachToTarget",
            &AttachToTargetParams {
                target_id,
                flatten: true,
            },
            None,
        )
        .await;

    match attached {
        Ok(result) => {
            if let Ok(mut guard) = shared.lock() {
                if let Some(capture) = guard.as_mut() {
                    capture.session_id = Some(result.session_id.clone());
                }
            }
            Ok(result.session_id)
        }
        Err(e) => {
            if let Ok(mut guard) = shared.lock() {
                *guard = None;
            }
            Err(format!("Failed to attach recording session: {}", e))
        }
    }
}

/// Detach a capture session that was attached for recording. This is best
/// effort because the page or browser may already be gone.
pub async fn detach_capture_session(client: &CdpClient, capture_session: &str) {
    let _ = tokio::time::timeout(
        TEARDOWN_TIMEOUT,
        client.send_command(
            "Target.detachFromTarget",
            Some(json!({ "sessionId": capture_session })),
            None,
        ),
    )
    .await;
}

fn ffmpeg_launch_error(error: impl std::fmt::Display) -> String {
    format!(
        "ffmpeg not found or failed to execute: {}. Install ffmpeg to enable recording.",
        error
    )
}

/// Verify that ffmpeg can run without opening or modifying the destination.
/// This keeps missing-binary failures ahead of browser attachment while the
/// real encoder process is deferred until that attachment succeeds.
pub async fn check_ffmpeg_available() -> Result<(), String> {
    let status = tokio::process::Command::new("ffmpeg")
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map_err(ffmpeg_launch_error)?;
    if !status.success() {
        return Err(ffmpeg_launch_error(status));
    }
    Ok(())
}

/// Start the encoder process after the browser capture session is ready.
pub fn spawn_ffmpeg(output_path: &str, fps: u32) -> Result<tokio::process::Child, String> {
    spawn_ffmpeg_command(&mut build_ffmpeg_command(output_path, fps))
}

fn spawn_ffmpeg_command(
    command: &mut tokio::process::Command,
) -> Result<tokio::process::Child, String> {
    command.spawn().map_err(ffmpeg_launch_error)
}

/// Spawn a background task that screencasts `capture_session` into the
/// already running `ffmpeg` at `fps`. Chrome pushes a frame on every repaint up to the display rate; a
/// wall-clock ticker queues one frame per slot, holding the last one through
/// gaps, so the file's duration matches the automation it recorded. A second
/// task drains the queue into ffmpeg, so encoding never delays capture.
/// Sending `true` on `cancel_rx` discards the take: ffmpeg is killed instead
/// of drained.
#[allow(clippy::too_many_arguments)]
pub fn spawn_recording_task(
    client: Arc<CdpClient>,
    capture_session: String,
    mut ffmpeg: tokio::process::Child,
    fps: u32,
    shared_count: Arc<AtomicU64>,
    shared_captured: Arc<AtomicU64>,
    cancel_rx: oneshot::Receiver<bool>,
    capture_gate: Option<Arc<RecordingCaptureGate>>,
) -> tokio::task::JoinHandle<Result<(), String>> {
    tokio::spawn(async move {
        let fps = validate_fps(fps)?;
        let period = frame_period(fps);
        let max_frames_per_tick = MAX_BACKFILL_SECS * fps as u64 + 1;

        // Frames go to a private channel so the daemon's other subscribers
        // neither copy them nor overflow on them. Subscribe before starting
        // the screencast: Chrome sends the first frame immediately.
        let events = client.subscribe_session(&capture_session);

        let stdin = ffmpeg
            .stdin
            .take()
            .ok_or_else(|| "Failed to open ffmpeg stdin".to_string())?;
        let (queue, runs) = EncoderQueue::new(fps);
        let writer = tokio::spawn(write_frames(runs, stdin));

        let started = client
            .send_command(
                "Page.startScreencast",
                Some(json!({
                    "format": "jpeg",
                    "quality": SCREENCAST_QUALITY,
                    // Always 1: Chrome skips frames by count, and a static
                    // page produces exactly one, which a higher value would
                    // drop, leaving nothing to record.
                    "everyNthFrame": 1,
                })),
                Some(&capture_session),
            )
            .await;

        let capture = match started {
            Ok(_) => {
                capture_frames(
                    &client,
                    &capture_session,
                    events,
                    queue,
                    period,
                    max_frames_per_tick,
                    &shared_count,
                    &shared_captured,
                    cancel_rx,
                    capture_gate.as_deref(),
                )
                .await
            }
            Err(e) => {
                drop(queue);
                Err(format!("Failed to start screencast: {}", e))
            }
        };

        client.unsubscribe_session(&capture_session);

        // Best effort: the page may already be closed.
        let _ = tokio::time::timeout(
            TEARDOWN_TIMEOUT,
            client.send_command_no_params("Page.stopScreencast", Some(&capture_session)),
        )
        .await;
        detach_capture_session(&client, &capture_session).await;

        // ffmpeg sees EOF once the writer has drained what capture queued.
        // A discarded take is not worth draining, and an encoder that has
        // stopped reading would hold `record stop` forever.
        let discard = matches!(capture, Ok(true));
        let drain_started = tokio::time::Instant::now();
        let drained = if discard {
            writer.abort();
            let _ = writer.await;
            Ok(())
        } else {
            drain_encoder(writer, ENCODER_DRAIN_LIMIT).await
        };
        if discard || drained.is_err() {
            let _ = ffmpeg.start_kill();
        }
        if std::env::var("AGENT_BROWSER_DEBUG").is_ok() {
            let _ = writeln!(
                std::io::stderr(),
                "[recording] encoder drained in {:?}",
                drain_started.elapsed()
            );
        }
        let output = ffmpeg
            .wait_with_output()
            .await
            .map_err(|e| format!("ffmpeg wait failed: {}", e))?;

        capture?;
        drained?;

        if !discard && !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("ffmpeg failed: {}", ffmpeg_error_tail(&stderr)));
        }

        Ok(())
    })
}

/// Pump screencast frames into the encoder queue until cancelled, the page
/// goes away, or the encoder hangs up. Takes ownership of `frames` so the
/// writer sees the end of the queue on return. Returns whether the take is
/// being discarded.
#[allow(clippy::too_many_arguments)]
async fn capture_frames(
    client: &CdpClient,
    capture_session: &str,
    mut events: mpsc::Receiver<super::cdp::types::CdpEvent>,
    frames: EncoderQueue,
    period: Duration,
    max_frames_per_tick: u64,
    shared_count: &AtomicU64,
    shared_captured: &AtomicU64,
    cancel_rx: oneshot::Receiver<bool>,
    capture_gate: Option<&RecordingCaptureGate>,
) -> Result<bool, String> {
    let mut cancel_rx = std::pin::pin!(cancel_rx);
    let mut interval = tokio::time::interval(period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Frames waiting to be written, in arrival order, and the last frame
    // written (repeated through gaps). Chrome's frame clock and the ticker
    // are not phase-locked, so a slot sometimes receives two frames and the
    // next none; the queue carries the spare across instead of dropping it
    // and repeating its predecessor.
    let mut pending: VecDeque<Arc<[u8]>> = VecDeque::new();
    let mut last: Option<Arc<[u8]>> = None;
    let mut segment_started: Option<tokio::time::Instant> = None;
    let mut segment_written: u64 = 0;
    let mut discard = false;
    let mut peak_backlog = 0;

    loop {
        tokio::select! {
            cancel = &mut cancel_rx => {
                discard = cancel.unwrap_or(false);
                break;
            }
            event = events.recv() => {
                let Some(event) = event else { break };
                if event.method == "Page.screencastFrame" {
                    if let Some(sid) = event.params.get("sessionId").and_then(Value::as_i64) {
                        let _ = client
                            .send_command_no_wait(
                                "Page.screencastFrameAck",
                                Some(json!({ "sessionId": sid })),
                                Some(capture_session),
                            )
                            .await;
                    }
                    let decoded = event
                        .params
                        .get("data")
                        .and_then(Value::as_str)
                        .and_then(|data| {
                            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data)
                                .ok()
                        });
                    if let Some(bytes) = decoded {
                        pending.push_back(Arc::from(bytes));
                        // Only a lower recording rate lets the queue grow (a
                        // 60 Hz screencast into a 30 fps file); dropping the
                        // oldest keeps the picture current.
                        if pending.len() > MAX_PENDING_FRAMES {
                            pending.pop_front();
                        }
                        shared_captured.fetch_add(1, Ordering::Relaxed);
                    }
                } else if event.method == "Inspector.detached" {
                    // The recorded page was closed; finish the file.
                    break;
                }
            }
            _ = interval.tick() => {
                if capture_gate.is_some_and(|gate| !gate.is_active()) {
                    segment_started = None;
                    segment_written = 0;
                    continue;
                }
                if pending.is_empty() && last.is_none() {
                    continue;
                }
                let now = tokio::time::Instant::now();
                let started = *segment_started.get_or_insert(now);
                let due = frames_due(now.duration_since(started), period, segment_written);
                if due == 0 {
                    continue;
                }
                // A gap longer than MAX_BACKFILL_SECS is held for that long
                // and the rest is dropped, so a hung page does not inflate
                // the file. Advancing `written` by the full amount is what
                // stops the excess being paid off on later ticks.
                let emit = due.min(max_frames_per_tick);
                // A stop must be heard while capture waits for room in the
                // backlog, or an encoder that stopped reading would hang it.
                let mut ended = None;
                for run in fill_slots(&mut pending, &mut last, emit) {
                    let slots = run.1;
                    tokio::select! {
                        sent = frames.send(run) => {
                            if sent.is_err() {
                                ended = Some(false);
                                break;
                            }
                        }
                        cancel = &mut cancel_rx => {
                            ended = Some(cancel.unwrap_or(false));
                            break;
                        }
                    }
                    shared_count.fetch_add(slots, Ordering::Relaxed);
                    peak_backlog = peak_backlog.max(frames.queued_slots());
                }
                if let Some(end) = ended {
                    discard = end;
                    break;
                }
                segment_written += due;
            }
        }
    }

    if std::env::var("AGENT_BROWSER_DEBUG").is_ok() {
        let _ = writeln!(
            std::io::stderr(),
            "[recording] encoder backlog peaked at {} of {} slots",
            peak_backlog,
            frames.capacity
        );
    }
    drop(frames);
    Ok(discard)
}

/// Write queued frames to ffmpeg until capture closes the queue or the pipe
/// breaks. Returning drops the queue, which is how a dead encoder ends
/// capture, and drops `stdin`, which is ffmpeg's EOF.
async fn write_frames<W>(mut runs: mpsc::UnboundedReceiver<QueuedRun>, mut stdin: W)
where
    W: tokio::io::AsyncWrite + Unpin,
{
    // The permit returns the run's slots to the backlog once it is written.
    while let Some(((frame, slots), _permit)) = runs.recv().await {
        for _ in 0..slots {
            if stdin.write_all(&frame).await.is_err() {
                return;
            }
        }
    }
}

/// Wait up to `limit` for the writer to finish, aborting it past that. Only
/// an encoder that has stopped reading its input takes that long.
async fn drain_encoder(
    mut writer: tokio::task::JoinHandle<()>,
    limit: Duration,
) -> Result<(), String> {
    if tokio::time::timeout(limit, &mut writer).await.is_ok() {
        return Ok(());
    }
    writer.abort();
    let _ = writer.await;
    Err(format!(
        "ffmpeg did not finish encoding the recording within {:?}",
        limit
    ))
}

/// Add recorded interaction sounds to a completed Chromium screencast. The
/// video stream is copied without re-encoding, so visual output remains the
/// exact output produced by the upstream recorder.
pub async fn finalize_recording_audio(state: &RecordingState) -> Result<(), String> {
    let mut events = state
        .sound_events
        .lock()
        .map_err(|_| "recording soundtrack state is unavailable".to_string())?
        .clone();
    if events.is_empty() || state.frame_count == 0 {
        return Ok(());
    }
    events.sort_by_key(|event| match event {
        SoundEvent::Click { frame } => *frame,
        SoundEvent::Keyboard { start_frame, .. } => *start_frame,
    });

    let output_path = PathBuf::from(&state.output_path);
    let extension = output_path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("webm");
    let video_path = temporary_recording_path(&output_path, "video", extension);
    let audio_path = temporary_recording_path(&output_path, "audio", "wav");
    let mux_path = temporary_recording_path(&output_path, "mux", extension);

    std::fs::rename(&output_path, &video_path).map_err(|error| {
        format!(
            "could not prepare recording for audio mux at {}: {}",
            output_path.display(),
            error
        )
    })?;

    let finish = async {
        write_soundtrack_wav(&audio_path, state.frame_count, state.fps, events.as_slice())?;
        mux_soundtrack(&video_path, &audio_path, &mux_path, extension).await?;
        std::fs::rename(&mux_path, &output_path).map_err(|error| {
            format!(
                "could not install recording with audio at {}: {}",
                output_path.display(),
                error
            )
        })
    }
    .await;

    let _ = remove_partial_file(&audio_path);
    let _ = remove_partial_file(&mux_path);
    match finish {
        Ok(()) => {
            let _ = remove_partial_file(&video_path);
            Ok(())
        }
        Err(error) => {
            if !output_path.exists() {
                let _ = std::fs::rename(&video_path, &output_path);
            }
            Err(error)
        }
    }
}

fn append_keyboard_sound(events: &mut Vec<SoundEvent>, start_frame: u64, end_frame: u64) {
    match events.last_mut() {
        Some(SoundEvent::Keyboard {
            end_frame: previous_end,
            ..
        }) if *previous_end == start_frame => *previous_end = end_frame,
        _ => events.push(SoundEvent::Keyboard {
            start_frame,
            end_frame,
        }),
    }
}

fn temporary_recording_path(path: &Path, stage: &str, extension: &str) -> PathBuf {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let nonce = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    parent.join(format!(
        ".agent-browser-recording-{}-{}-{}.{}",
        std::process::id(),
        nonce,
        stage,
        extension
    ))
}

fn remove_partial_file(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("could not remove partial recording: {}", error)),
    }
}

async fn mux_soundtrack(
    video: &Path,
    audio: &Path,
    output: &Path,
    extension: &str,
) -> Result<(), String> {
    let mut command = tokio::process::Command::new("ffmpeg");
    command
        .args(["-y", "-loglevel", "error", "-nostats"])
        .arg("-i")
        .arg(video)
        .arg("-i")
        .arg(audio)
        .args(["-map", "0:v:0", "-map", "1:a:0", "-c:v", "copy"]);
    if extension.eq_ignore_ascii_case("webm") {
        command.args(["-c:a", "libopus", "-b:a", "96k"]);
    } else {
        command.args(["-c:a", "aac", "-b:a", "128k"]);
    }
    let result = command
        .args([
            "-shortest",
            "-metadata:s:a:0",
            "title=Recorded interaction effects",
        ])
        .arg(output)
        .output()
        .await
        .map_err(|error| format!("could not start ffmpeg audio mux: {}", error))?;
    if result.status.success() {
        Ok(())
    } else {
        Err(format!(
            "ffmpeg audio mux failed: {}",
            String::from_utf8_lossy(&result.stderr).trim()
        ))
    }
}

fn write_soundtrack_wav(
    path: &Path,
    video_frames: u64,
    fps: u32,
    events: &[SoundEvent],
) -> Result<(), String> {
    let total_samples = frame_to_sample(video_frames, fps)?;
    let data_bytes = total_samples
        .checked_mul(u64::from(AUDIO_CHANNELS) * 2)
        .filter(|bytes| *bytes <= u64::from(u32::MAX) - 36)
        .ok_or("recording is too long for its temporary WAV soundtrack")?
        as u32;
    let file = File::create(path)
        .map_err(|error| format!("could not create temporary soundtrack: {}", error))?;
    let mut writer = BufWriter::new(file);
    write_wav_header(&mut writer, data_bytes)?;

    let scheduled = events
        .iter()
        .map(|event| match *event {
            SoundEvent::Click { frame } => {
                let start = frame_to_sample(frame, fps)?;
                Ok((
                    start,
                    start.saturating_add(embedded_wav_sample_count(CLICK_WAV)),
                    RecordedSound::Click,
                ))
            }
            SoundEvent::Keyboard {
                start_frame,
                end_frame,
            } => Ok((
                frame_to_sample(start_frame, fps)?,
                frame_to_sample(end_frame, fps)?,
                RecordedSound::Keyboard,
            )),
        })
        .collect::<Result<Vec<_>, String>>()?;
    let mut next_event = 0;
    let mut active = Vec::<(u64, u64, RecordedSound)>::new();
    for sample in 0..total_samples {
        while next_event < scheduled.len() && scheduled[next_event].0 <= sample {
            active.push(scheduled[next_event]);
            next_event += 1;
        }
        active.retain(|(_, end, _)| sample < *end);
        let mut left = 0.0;
        let mut right = 0.0;
        for (start, end, sound) in &active {
            let (event_left, event_right) =
                recorded_sound_sample(*sound, sample - *start, end - start);
            left += event_left;
            right += event_right;
        }
        for value in [left, right] {
            let encoded = (value.clamp(-0.92, 0.92) * f32::from(i16::MAX)).round() as i16;
            writer
                .write_all(&encoded.to_le_bytes())
                .map_err(|error| format!("could not write temporary soundtrack: {}", error))?;
        }
    }
    writer
        .flush()
        .map_err(|error| format!("could not finish temporary soundtrack: {}", error))
}

fn write_wav_header(writer: &mut impl Write, data_bytes: u32) -> Result<(), String> {
    let byte_rate = AUDIO_SAMPLE_RATE * u32::from(AUDIO_CHANNELS) * 2;
    let block_align = AUDIO_CHANNELS * 2;
    let mut header = Vec::with_capacity(44);
    header.extend_from_slice(b"RIFF");
    header.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    header.extend_from_slice(b"WAVEfmt ");
    header.extend_from_slice(&16u32.to_le_bytes());
    header.extend_from_slice(&1u16.to_le_bytes());
    header.extend_from_slice(&AUDIO_CHANNELS.to_le_bytes());
    header.extend_from_slice(&AUDIO_SAMPLE_RATE.to_le_bytes());
    header.extend_from_slice(&byte_rate.to_le_bytes());
    header.extend_from_slice(&block_align.to_le_bytes());
    header.extend_from_slice(&16u16.to_le_bytes());
    header.extend_from_slice(b"data");
    header.extend_from_slice(&data_bytes.to_le_bytes());
    writer
        .write_all(&header)
        .map_err(|error| format!("could not write temporary soundtrack header: {}", error))
}

fn frame_to_sample(frame: u64, fps: u32) -> Result<u64, String> {
    if fps == 0 {
        return Err("recording fps must be positive".into());
    }
    u64::try_from(
        (u128::from(frame) * u128::from(AUDIO_SAMPLE_RATE) + u128::from(fps / 2)) / u128::from(fps),
    )
    .map_err(|_| "recording is too long to synchronize its soundtrack".into())
}

fn embedded_wav_sample_count(wav: &[u8]) -> u64 {
    debug_assert_eq!(&wav[..4], b"RIFF");
    debug_assert_eq!(&wav[8..12], b"WAVE");
    debug_assert_eq!(&wav[36..40], b"data");
    ((wav.len() - EMBEDDED_WAV_HEADER_LEN) / 2) as u64
}

fn embedded_wav_sample(wav: &[u8], sample: u64) -> f32 {
    let offset = EMBEDDED_WAV_HEADER_LEN + sample as usize * 2;
    if offset + 2 > wav.len() {
        return 0.0;
    }
    f32::from(i16::from_le_bytes([wav[offset], wav[offset + 1]])) / f32::from(i16::MAX)
}

fn recorded_sound_sample(sound: RecordedSound, sample: u64, duration: u64) -> (f32, f32) {
    if sample >= duration {
        return (0.0, 0.0);
    }
    let mono = match sound {
        RecordedSound::Click => embedded_wav_sample(CLICK_WAV, sample),
        RecordedSound::Keyboard => {
            let fade_samples = KEYBOARD_EDGE_FADE_SAMPLES.min(duration / 2).max(1);
            let fade_in = (sample as f32 / fade_samples as f32).min(1.0);
            let remaining = duration.saturating_sub(sample + 1);
            let fade_out = (remaining as f32 / fade_samples as f32).min(1.0);
            looped_keyboard_sample(sample) * KEYBOARD_GAIN * fade_in * fade_out
        }
    };
    (mono, mono)
}

fn looped_keyboard_sample(sample: u64) -> f32 {
    let sample_count = embedded_wav_sample_count(KEYBOARD_WAV);
    let crossfade = KEYBOARD_CROSSFADE_SAMPLES.min(sample_count / 4);
    let period = sample_count - crossfade;
    if sample < period {
        return embedded_wav_sample(KEYBOARD_WAV, sample);
    }
    let position = sample % period;
    if position >= crossfade {
        return embedded_wav_sample(KEYBOARD_WAV, position);
    }
    let progress = position as f32 / crossfade.max(1) as f32;
    let tail = embedded_wav_sample(KEYBOARD_WAV, period + position);
    let head = embedded_wav_sample(KEYBOARD_WAV, position);
    tail * (1.0 - progress) + head * progress
}

pub async fn stop_recording_task(state: &mut RecordingState) -> Result<(), String> {
    end_recording_task(state, false).await
}

/// Stop the capture task of a take that is being thrown away, killing the
/// encoder instead of waiting for it to finish the file.
pub async fn discard_recording_task(state: &mut RecordingState) -> Result<(), String> {
    end_recording_task(state, true).await
}

async fn end_recording_task(state: &mut RecordingState, discard: bool) -> Result<(), String> {
    if let Some(tx) = state.cancel_tx.take() {
        let _ = tx.send(discard);
    }

    let counter = state.shared_frame_count.take();
    let captured = state.shared_captured_count.take();
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
    if let Some(c) = captured {
        state.captured_count = c.load(Ordering::Relaxed);
    }
    if let Ok(mut guard) = state.capture_session.lock() {
        *guard = None;
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
        assert_eq!(state.fps, DEFAULT_FPS);
    }

    #[test]
    fn test_recording_start_sets_active() {
        let mut state = RecordingState::new();
        let result = recording_start(&mut state, "/tmp/test.mp4", None);
        assert!(result.is_ok());
        assert!(state.active);
        assert_eq!(state.output_path, "/tmp/test.mp4");
        assert_eq!(state.frame_count, 0);
        assert_eq!(state.fps, 30);
        assert_eq!(result.unwrap()["fps"], 30);
    }

    #[test]
    fn test_recording_start_honors_requested_fps() {
        let mut state = RecordingState::new();
        let result = recording_start(&mut state, "/tmp/test.webm", Some(60)).unwrap();
        assert_eq!(state.fps, 60);
        assert_eq!(result["fps"], 60);
    }

    #[test]
    fn test_recording_start_rejects_out_of_range_fps() {
        let mut state = RecordingState::new();
        let too_high = recording_start(&mut state, "/tmp/test.webm", Some(61));
        assert!(too_high.unwrap_err().contains("valid range: 1-60"));
        assert!(!state.active);

        let zero = recording_start(&mut state, "/tmp/test.webm", Some(0));
        assert!(zero.is_err());
        assert!(!state.active);
    }

    #[test]
    fn test_recording_start_rejects_extensionless_path() {
        let mut state = RecordingState::new();
        for path in [
            "/tmp/take",
            "/tmp/dir.v2/take",
            "/tmp/.hidden",
            "/tmp/take.",
        ] {
            let err = recording_start(&mut state, path, None).unwrap_err();
            assert!(err.contains(path), "error should name the path: {}", err);
            assert!(err.contains("no extension"), "error was: {}", err);
            assert!(err.contains(".webm"), "error should suggest .webm: {}", err);
            assert!(err.contains(".mp4"), "error should suggest .mp4: {}", err);
            assert!(!state.active);
        }
    }

    #[test]
    fn test_validate_output_path_accepts_any_extension() {
        // Only the two documented formats are tuned, but ffmpeg muxes
        // libx264 into .mkv/.mov/.avi fine and those worked before the
        // check existed, so anything with an extension passes.
        for path in [
            "take.webm",
            "take.mp4",
            "./out/TAKE.WEBM",
            "/abs/path/Take.Mp4",
            "dotted.name.webm",
            "take.mkv",
            "take.mov",
            "take.avi",
            "take.webm.part",
            "dir.v2/take.MP4",
        ] {
            assert!(validate_output_path(path).is_ok(), "{} should pass", path);
        }
    }

    #[test]
    fn test_validate_output_path_rejects_missing_extension() {
        for path in ["take", "take.", ".hidden", ".webm", "dir.v2/take", "dir/"] {
            assert!(validate_output_path(path).is_err(), "{} should fail", path);
        }
        assert_eq!(
            validate_output_path("take").unwrap_err(),
            "Invalid output path: 'take' has no extension (use .webm or .mp4)"
        );
    }

    #[test]
    fn test_build_ffmpeg_command_matches_extension_case_insensitively() {
        let cmd = build_ffmpeg_command("/tmp/OUT.WEBM", DEFAULT_FPS);
        let args: Vec<&std::ffi::OsStr> = cmd.as_std().get_args().collect();
        let args_str: Vec<&str> = args.iter().filter_map(|a| a.to_str()).collect();
        assert!(args_str.contains(&"libvpx"));
        assert!(!args_str.contains(&"libx264"));
    }

    #[test]
    fn test_build_ffmpeg_command_hides_banner() {
        let cmd = build_ffmpeg_command("/tmp/out.webm", DEFAULT_FPS);
        let args: Vec<&std::ffi::OsStr> = cmd.as_std().get_args().collect();
        assert!(args.contains(&std::ffi::OsStr::new("-hide_banner")));
    }

    #[test]
    fn test_ffmpeg_error_tail_keeps_short_output_whole() {
        let stderr = "Unable to choose an output format for 'take'\nError opening output files: Invalid argument\n";
        assert_eq!(
            ffmpeg_error_tail(stderr),
            "Unable to choose an output format for 'take'\nError opening output files: Invalid argument"
        );
    }

    #[test]
    fn test_ffmpeg_error_tail_keeps_the_cause_not_the_banner() {
        // A realistic failure: banner and configure line first, the cause
        // last. The old 300-character head never reached the cause.
        let banner = format!(
            "ffmpeg version 8.1 Copyright (c) 2000-2026 the FFmpeg developers\n  built with clang\n  configuration: {}\n",
            "--enable-libx264 ".repeat(40)
        );
        let cause = "[out#0 @ 0x1] Unable to choose an output format for './take'; use a specific format\nError opening output file ./take.\nError opening output files: Invalid argument";
        let stderr = format!("{}{}\n", banner, cause);

        let tail = ffmpeg_error_tail(&stderr);
        assert!(tail.ends_with(cause), "tail was: {}", tail);
        assert!(!tail.contains("ffmpeg version"), "tail was: {}", tail);
        assert!(tail.chars().count() <= FFMPEG_STDERR_TAIL_CHARS);
        // The window opened mid-way through the configure line; that
        // partial line is dropped so the message starts on a whole one.
        assert!(tail.starts_with("[out#0"), "tail was: {}", tail);
    }

    #[test]
    fn test_ffmpeg_error_tail_keeps_a_single_long_line() {
        let line = "x".repeat(FFMPEG_STDERR_TAIL_CHARS + 50);
        let tail = ffmpeg_error_tail(&line);
        assert_eq!(tail.chars().count(), FFMPEG_STDERR_TAIL_CHARS);
    }

    #[test]
    fn test_ffmpeg_error_tail_handles_multibyte_and_progress_lines() {
        // ffmpeg's progress output uses carriage returns; the cut must not
        // split a multi-byte character.
        let stderr = format!(
            "{}\rframe=   12 fps=0.0 q=0.0\rError: ✗ muxer",
            "é".repeat(500)
        );
        let tail = ffmpeg_error_tail(&stderr);
        assert!(tail.ends_with("Error: ✗ muxer"), "tail was: {}", tail);
        assert!(tail.starts_with("frame="), "tail was: {}", tail);
    }

    #[test]
    fn test_recording_start_while_active() {
        let mut state = RecordingState::new();
        recording_start(&mut state, "/tmp/test1.mp4", None).unwrap();
        let result = recording_start(&mut state, "/tmp/test2.mp4", None);
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
        recording_start(&mut state, "/tmp/test.mp4", None).unwrap();
        let result = recording_stop(&mut state);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("No frames"));
        assert!(!state.active);
    }

    #[test]
    fn test_recording_stop_reports_fps() {
        let mut state = RecordingState::new();
        recording_start(&mut state, "/tmp/test.webm", Some(60)).unwrap();
        state.frame_count = 120;
        let result = recording_stop(&mut state).unwrap();
        assert_eq!(result["frames"], 120);
        assert_eq!(result["fps"], 60);
    }

    #[test]
    fn test_capture_session_matches_by_target_while_attach_is_in_flight() {
        let shared: SharedCaptureSession = Arc::new(Mutex::new(Some(CaptureSession {
            target_id: "T-REC".into(),
            session_id: None,
        })));
        assert!(owns_attachment(&shared, "T-REC", "S-ANY"));
        assert!(!owns_attachment(&shared, "T-OTHER", "S-ANY"));
    }

    #[test]
    fn test_capture_session_matches_by_session_once_attached() {
        let shared: SharedCaptureSession = Arc::new(Mutex::new(Some(CaptureSession {
            target_id: "T-REC".into(),
            session_id: Some("S-REC".into()),
        })));
        assert!(owns_attachment(&shared, "T-REC", "S-REC"));
        // A later attachment to the same target (e.g. the daemon's own) is
        // not the recorder's.
        assert!(!owns_attachment(&shared, "T-REC", "S-OTHER"));
        assert!(!owns_attachment(
            &Arc::new(Mutex::new(None)),
            "T-REC",
            "S-REC"
        ));
    }

    #[test]
    fn test_validate_fps_range() {
        assert_eq!(validate_fps(1).unwrap(), 1);
        assert_eq!(validate_fps(DEFAULT_FPS).unwrap(), 30);
        assert_eq!(validate_fps(MAX_FPS).unwrap(), 60);
        assert!(validate_fps(0).is_err());
        assert!(validate_fps(MAX_FPS + 1).is_err());
    }

    #[test]
    fn test_frame_period_matches_fps() {
        assert_eq!(frame_period(1), Duration::from_millis(1000));
        assert_eq!(frame_period(30), Duration::from_micros(33333));
        assert_eq!(frame_period(60), Duration::from_micros(16666));
    }

    #[test]
    fn test_frames_due_on_schedule_emits_one_frame() {
        let period = frame_period(30);
        for slot in 0..5u64 {
            let elapsed = period * slot as u32;
            assert_eq!(frames_due(elapsed, period, slot), 1);
        }
    }

    #[test]
    fn test_frames_due_is_zero_when_slot_already_written() {
        let period = frame_period(60);
        // Two screencast frames in one slot: the second waits for the next
        // tick rather than stretching the file.
        assert_eq!(frames_due(period / 2, period, 1), 0);
        assert_eq!(frames_due(Duration::ZERO, period, 10), 0);
    }

    #[test]
    fn test_frames_due_backfills_missed_slots() {
        let period = frame_period(60);
        // The ticker wakes in slot 3 with only slot 0 written, so the two
        // skipped slots are held along with the current one.
        assert_eq!(frames_due(period * 3, period, 1), 3);
    }

    /// Replays the ticker's bookkeeping for a 60s stall followed by on-time
    /// ticks. The cap must bound the file, not just one tick: without
    /// advancing `written` by the full deficit, every later tick would emit
    /// another five seconds of held frames until the stall was paid off.
    #[test]
    fn test_backfill_cap_bounds_a_long_stall() {
        let fps = 30u32;
        let period = frame_period(fps);
        let max_frames = MAX_BACKFILL_SECS * fps as u64 + 1;
        let mut written = 0u64;
        let mut emitted = 0u64;

        let mut elapsed = Duration::from_secs(60);
        let due = frames_due(elapsed, period, written);
        assert!(due > max_frames);
        emitted += due.min(max_frames);
        written += due;

        for _ in 0..20 {
            elapsed += period;
            let due = frames_due(elapsed, period, written);
            assert_eq!(due, 1, "ticks after the stall must emit one frame each");
            emitted += due.min(max_frames);
            written += due;
        }

        assert_eq!(emitted, max_frames + 20);
    }

    #[tokio::test]
    async fn test_spawn_ffmpeg_reports_missing_binary() {
        let mut command = tokio::process::Command::new("agent-browser-no-such-ffmpeg");
        let err = spawn_ffmpeg_command(&mut command).unwrap_err();
        assert!(err.contains("ffmpeg not found"), "{err}");
        assert!(err.contains("Install ffmpeg"), "{err}");
    }

    #[test]
    fn test_build_ffmpeg_command_webm() {
        let cmd = build_ffmpeg_command("/tmp/out.webm", DEFAULT_FPS);
        let args: Vec<&std::ffi::OsStr> = cmd.as_std().get_args().collect();
        let args_str: Vec<&str> = args.iter().filter_map(|a| a.to_str()).collect();
        assert!(args_str.contains(&"libvpx"));
        assert!(args_str.contains(&"/tmp/out.webm"));
        assert!(args_str.contains(&"1000k"));
    }

    #[test]
    fn test_build_ffmpeg_command_mp4() {
        let cmd = build_ffmpeg_command("/tmp/out.mp4", DEFAULT_FPS);
        let args: Vec<&std::ffi::OsStr> = cmd.as_std().get_args().collect();
        let args_str: Vec<&str> = args.iter().filter_map(|a| a.to_str()).collect();
        assert!(args_str.contains(&"libx264"));
        assert!(args_str.contains(&"/tmp/out.mp4"));
    }

    #[test]
    fn test_build_ffmpeg_command_passes_framerate() {
        let cmd = build_ffmpeg_command("/tmp/out.webm", 60);
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .filter_map(|a| a.to_str().map(String::from))
            .collect();
        let framerate = args
            .iter()
            .position(|a| a == "-framerate")
            .and_then(|i| args.get(i + 1))
            .map(String::as_str);
        assert_eq!(framerate, Some("60"));
        // 60 fps doubles the VP8 bitrate budget and adds an encoder thread.
        assert!(args.iter().any(|a| a == "2000k"));
        let threads = args
            .iter()
            .position(|a| a == "-threads")
            .and_then(|i| args.get(i + 1))
            .map(String::as_str);
        assert_eq!(threads, Some("2"));
    }

    #[test]
    fn test_build_ffmpeg_command_single_thread_at_default_fps() {
        let cmd = build_ffmpeg_command("/tmp/out.mp4", DEFAULT_FPS);
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .filter_map(|a| a.to_str().map(String::from))
            .collect();
        let threads = args
            .iter()
            .position(|a| a == "-threads")
            .and_then(|i| args.get(i + 1))
            .map(String::as_str);
        assert_eq!(threads, Some("1"));
    }

    fn ffmpeg_args(output_path: &str, fps: u32) -> Vec<String> {
        build_ffmpeg_command(output_path, fps)
            .as_std()
            .get_args()
            .filter_map(|a| a.to_str().map(String::from))
            .collect()
    }

    fn arg_after<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .map(String::as_str)
    }

    #[test]
    fn test_build_ffmpeg_command_webm_encodes_on_a_realtime_budget() {
        for fps in [DEFAULT_FPS, MAX_FPS] {
            let args = ffmpeg_args("/tmp/out.webm", fps);
            assert_eq!(arg_after(&args, "-deadline"), Some("realtime"));
            assert_eq!(arg_after(&args, "-cpu-used"), Some("8"));
            // Quality and bitrate targets are unchanged by the deadline.
            assert_eq!(arg_after(&args, "-crf"), Some("30"));
        }
        // libx264 is already on its fastest preset; libvpx options stay off it.
        let args = ffmpeg_args("/tmp/out.mp4", DEFAULT_FPS);
        assert_eq!(arg_after(&args, "-preset"), Some("ultrafast"));
        assert!(!args.iter().any(|a| a == "-deadline" || a == "-cpu-used"));
    }

    #[test]
    fn test_build_ffmpeg_command_suppresses_progress_stats() {
        // stderr is read only after ffmpeg exits, so periodic progress lines
        // would eventually fill the pipe and block the encoder.
        for path in ["/tmp/out.webm", "/tmp/out.mp4"] {
            let args = ffmpeg_args(path, DEFAULT_FPS);
            assert!(args.iter().any(|a| a == "-nostats"), "{path}");
        }
    }

    fn frame(bytes: &[u8]) -> Arc<[u8]> {
        Arc::from(bytes)
    }

    fn slot_runs(queued: &[SlotRun]) -> Vec<(&[u8], u64)> {
        queued.iter().map(|(f, n)| (&f[..], *n)).collect()
    }

    #[test]
    fn test_fill_slots_holds_the_last_frame_through_a_gap_as_one_run() {
        let mut pending = VecDeque::new();
        let mut last = Some(frame(b"A"));
        let queued = fill_slots(&mut pending, &mut last, MAX_BACKFILL_SECS * 30 + 1);
        assert_eq!(
            slot_runs(&queued),
            vec![(&b"A"[..], MAX_BACKFILL_SECS * 30 + 1)]
        );
    }

    #[test]
    fn test_fill_slots_writes_pending_frames_in_order_before_holding() {
        let mut pending = VecDeque::from([frame(b"A"), frame(b"B")]);
        let mut last = Some(frame(b"Z"));
        let queued = fill_slots(&mut pending, &mut last, 4);
        assert_eq!(slot_runs(&queued), vec![(&b"A"[..], 1), (&b"B"[..], 3)]);
        assert!(pending.is_empty());
        assert_eq!(last.as_deref(), Some(&b"B"[..]));
    }

    #[test]
    fn test_fill_slots_carries_a_spare_frame_to_the_next_tick() {
        let mut pending = VecDeque::from([frame(b"A"), frame(b"B")]);
        let mut last = None;
        assert_eq!(
            slot_runs(&fill_slots(&mut pending, &mut last, 1)),
            vec![(&b"A"[..], 1)]
        );
        assert_eq!(
            slot_runs(&fill_slots(&mut pending, &mut last, 1)),
            vec![(&b"B"[..], 1)]
        );
        assert!(fill_slots(&mut VecDeque::new(), &mut None, 3).is_empty());
    }

    /// The timeline is decided by the ticker, not the encoder: every slot a
    /// tick owes is queued exactly once, whatever mix of new and held frames
    /// fills it.
    #[test]
    fn test_fill_slots_queues_exactly_the_slots_owed() {
        for (arrived, emit) in [(0, 1), (0, 7), (1, 1), (1, 5), (2, 1), (2, 2), (2, 9)] {
            let mut pending: VecDeque<_> = (0..arrived).map(|i| frame(&[i])).collect();
            let mut last = Some(frame(b"held"));
            let queued = fill_slots(&mut pending, &mut last, emit);
            let slots: u64 = queued.iter().map(|(_, n)| n).sum();
            assert_eq!(slots, emit, "{arrived} arrived, {emit} owed");
        }
    }

    #[test]
    fn test_encoder_backlog_covers_the_same_time_at_every_rate() {
        assert_eq!(encoder_backlog_slots(1), 5);
        assert_eq!(encoder_backlog_slots(DEFAULT_FPS), 150);
        assert_eq!(encoder_backlog_slots(MAX_FPS), 300);
        assert_eq!(encoder_backlog_slots(0), encoder_backlog_slots(1));
    }

    #[tokio::test]
    async fn test_write_frames_expands_runs_in_order_then_closes_the_pipe() {
        use tokio::io::AsyncReadExt;

        let (queue, runs) = EncoderQueue::new(DEFAULT_FPS);
        let slots = Arc::clone(&queue.slots);
        let (pipe, mut ffmpeg) = tokio::io::duplex(64);
        let writer = tokio::spawn(write_frames(runs, pipe));
        queue.send((frame(b"A"), 1)).await.unwrap();
        queue.send((frame(b"B"), 3)).await.unwrap();
        drop(queue);

        let mut received = Vec::new();
        ffmpeg.read_to_end(&mut received).await.unwrap();
        writer.await.unwrap();
        assert_eq!(received, b"ABBB");
        // Written runs hand their slots back to the backlog.
        assert_eq!(
            slots.available_permits(),
            encoder_backlog_slots(DEFAULT_FPS) as usize
        );
    }

    #[tokio::test]
    async fn test_write_frames_hangs_up_when_ffmpeg_goes_away() {
        let (queue, runs) = EncoderQueue::new(DEFAULT_FPS);
        let (pipe, ffmpeg) = tokio::io::duplex(64);
        drop(ffmpeg);
        let writer = tokio::spawn(write_frames(runs, pipe));
        queue.send((frame(b"A"), 1)).await.unwrap();
        writer.await.unwrap();
        // A closed queue is what makes capture stop.
        assert!(queue.send((frame(b"B"), 1)).await.is_err());
    }

    /// The failure this queue exists for: ffmpeg stops reading for a while.
    /// Capture must still be able to queue (and so keep acknowledging
    /// screencast frames) until the backlog is full, and only then wait.
    #[tokio::test]
    async fn test_stalled_encoder_does_not_hold_up_capture_until_backlog_is_full() {
        use futures_util::FutureExt;

        let (queue, runs) = EncoderQueue::new(DEFAULT_FPS);
        // A pipe that takes one byte and is then never read.
        let (pipe, _unread) = tokio::io::duplex(1);
        let writer = tokio::spawn(write_frames(runs, pipe));

        for _ in 0..encoder_backlog_slots(DEFAULT_FPS) {
            queue
                .send((frame(b"jpeg"), 1))
                .now_or_never()
                .expect("capture should queue while the encoder is stalled")
                .unwrap();
        }
        assert!(
            queue.send((frame(b"jpeg"), 1)).now_or_never().is_none(),
            "a full backlog makes capture wait"
        );
        writer.abort();
    }

    /// A frame held through a gap is one entry but one encode per slot, so
    /// the backlog counts slots: the longest hold a tick can queue fills it
    /// as surely as that many distinct frames.
    #[tokio::test]
    async fn test_encoder_queue_bounds_held_frames_by_slots() {
        use futures_util::FutureExt;

        let backlog = encoder_backlog_slots(DEFAULT_FPS);
        let longest_hold = MAX_BACKFILL_SECS * DEFAULT_FPS as u64 + 1;
        let (queue, runs) = EncoderQueue::new(DEFAULT_FPS);
        let (pipe, _unread) = tokio::io::duplex(1);
        let writer = tokio::spawn(write_frames(runs, pipe));

        queue
            .send((frame(b"held"), longest_hold))
            .now_or_never()
            .expect("an empty backlog takes even the longest hold")
            .unwrap();
        assert_eq!(queue.queued_slots(), backlog);
        for _ in 0..3 {
            assert!(
                queue.send((frame(b"next"), 1)).now_or_never().is_none(),
                "a backlog full of one held frame makes capture wait"
            );
        }
        assert_eq!(queue.queued_slots(), backlog);
        writer.abort();
    }

    #[tokio::test]
    async fn test_drain_encoder_gives_up_on_an_encoder_that_stopped_reading() {
        let (queue, runs) = EncoderQueue::new(DEFAULT_FPS);
        let (pipe, _unread) = tokio::io::duplex(1);
        let writer = tokio::spawn(write_frames(runs, pipe));
        queue.send((frame(b"jpeg"), 1)).await.unwrap();
        drop(queue);

        let limit = Duration::from_millis(50);
        let drained = tokio::time::timeout(limit * 20, drain_encoder(writer, limit))
            .await
            .expect("the drain must end at its limit");
        assert!(drained.unwrap_err().contains("did not finish encoding"));
    }

    #[test]
    fn test_embedded_recording_sounds_are_supported_pcm_wav() {
        for wav in [CLICK_WAV, KEYBOARD_WAV] {
            assert_eq!(&wav[..4], b"RIFF");
            assert_eq!(&wav[8..12], b"WAVE");
            assert_eq!(u16::from_le_bytes([wav[20], wav[21]]), 1);
            assert_eq!(u16::from_le_bytes([wav[22], wav[23]]), 1);
            assert_eq!(
                u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]),
                AUDIO_SAMPLE_RATE
            );
            assert_eq!(u16::from_le_bytes([wav[34], wav[35]]), 16);
            assert!(embedded_wav_sample_count(wav) > 0);
        }
    }

    #[test]
    fn test_frame_to_sample_keeps_audio_on_the_video_clock() {
        assert_eq!(frame_to_sample(0, 30).unwrap(), 0);
        assert_eq!(frame_to_sample(30, 30).unwrap(), 48_000);
        assert_eq!(frame_to_sample(60, 60).unwrap(), 48_000);
        assert_eq!(frame_to_sample(1, 24).unwrap(), 2_000);
        assert!(frame_to_sample(1, 0).is_err());
    }

    #[test]
    fn test_adjacent_keyboard_events_form_one_sound_region() {
        let mut events = Vec::new();
        append_keyboard_sound(&mut events, 2, 5);
        append_keyboard_sound(&mut events, 5, 9);
        append_keyboard_sound(&mut events, 11, 12);
        assert_eq!(
            events,
            vec![
                SoundEvent::Keyboard {
                    start_frame: 2,
                    end_frame: 9,
                },
                SoundEvent::Keyboard {
                    start_frame: 11,
                    end_frame: 12,
                },
            ]
        );
    }

    #[test]
    fn test_sound_handle_schedules_click_and_keyboard_against_written_frames() {
        let mut state = RecordingState::new();
        recording_start(&mut state, "/tmp/test.webm", Some(30)).unwrap();
        let handle = state.sound_handle().unwrap();
        state
            .shared_frame_count
            .as_ref()
            .unwrap()
            .store(45, Ordering::Relaxed);
        handle.click();
        handle.keyboard_ending_now(Duration::from_millis(500));
        assert_eq!(
            *state.sound_events.lock().unwrap(),
            vec![
                SoundEvent::Click { frame: 45 },
                SoundEvent::Keyboard {
                    start_frame: 30,
                    end_frame: 45,
                },
            ]
        );
    }

    #[test]
    fn test_soundtrack_wav_matches_video_duration() {
        let path = temporary_recording_path(Path::new("soundtrack.webm"), "test", "wav");
        let result = write_soundtrack_wav(
            &path,
            30,
            30,
            &[
                SoundEvent::Click { frame: 2 },
                SoundEvent::Keyboard {
                    start_frame: 10,
                    end_frame: 20,
                },
            ],
        );
        assert!(result.is_ok());
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(bytes.len(), 44 + 48_000 * 2 * 2);
        assert!(bytes[44..].iter().any(|byte| *byte != 0));
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn test_demo_capture_gate_activates_and_expires() {
        let gate = RecordingCaptureGate::new_paused();
        assert!(!gate.is_active());
        gate.activate_for(Duration::from_millis(1)).await;
        assert!(gate.is_active());
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert!(!gate.is_active());
    }

    #[tokio::test]
    async fn test_demo_capture_gate_stays_active_until_action_ends() {
        let gate = RecordingCaptureGate::new_paused();
        gate.begin_action().await;
        assert!(gate.is_active());
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert!(gate.is_active());

        gate.end_action(Duration::from_millis(2)).await;
        assert!(gate.is_active());
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert!(!gate.is_active());
    }
}
