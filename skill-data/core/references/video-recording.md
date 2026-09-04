# Video Recording

Capture browser automation as video for debugging, documentation, or verification.

**Related**: [commands.md](commands.md) for full command reference, [SKILL.md](../SKILL.md) for quick start.

## Contents

- [Basic Recording](#basic-recording)
- [Recording Commands](#recording-commands)
- [Frame Rate](#frame-rate)
- [Recording Effects](#recording-effects)
- [Sound Effects](#sound-effects)
- [Use Cases](#use-cases)
- [Best Practices](#best-practices)
- [Output Format](#output-format)
- [Limitations](#limitations)

## Basic Recording

```bash
# Launch the browser, then start recording
agent-browser open https://example.com
agent-browser record start ./demo.webm

# Perform actions
agent-browser snapshot -i
agent-browser click @e1
agent-browser fill @e2 "test input"

# Stop and save
agent-browser record stop
```

## Recording Commands

```bash
# Launch a session first
agent-browser open

# Start recording to file (30 fps)
agent-browser record start ./output.webm

# Start recording at a specific rate (1-60)
agent-browser record start ./output.webm --fps 60

# Stop current recording
agent-browser record stop

# Restart with new file (stops current + starts new)
agent-browser record restart ./take2.webm --fps 60
```

## Frame Rate

Recording captures 30 fps by default, so scrolling, hover states, and CSS transitions read as motion instead of a slideshow. `--fps` takes any rate from 1 to 60.

| Rate | Use it for |
| --- | --- |
| 60 | Short, motion-heavy takes: drag interactions, animation, scroll polish work |
| 30 (default) | Flows, CI evidence, walkthroughs |
| 1-15 | Long sessions where the video is a timeline, not a motion study |

```bash
# Animation review
agent-browser record start ./transition.webm --fps 60
agent-browser click @e1
agent-browser wait 1500
agent-browser record stop

# Hour-long soak run
agent-browser record start ./soak.webm --fps 5
```

Frames come from Chrome's screencast, so a 60 fps take of a scroll holds 60 distinct pictures per second. While the page is static the last frame is held, so duration matches wall clock; a gap longer than five seconds is held for five and the rest left out. `record stop --json` reports `frames` (written) and `capturedFrames` (distinct frames the page produced). 60 fps roughly doubles the bitrate of 30 fps.

## Recording Effects

The OS cursor is never visible in `record start` output because Chromium's `Page.startScreencast` captures page frames without the OS cursor. Recording effects are **on by default** with the `cursor` preset and `arrow` theme. Recording uses a fresh browser context, copies cookies from the active session, and injects effects into the recording page. The effects are registered for future documents while recording is active, so they come back after navigation. Pass `--record-mode demo` for presentation timing defaults. Pass `--record-effects off` or `--no-cursor` to disable synthetic effects.

```bash
# Default: arrow cursor, 250ms tween, 400ms click ripple, 28px
agent-browser record start ./demo.webm https://example.com

# Demo timing defaults: blocking click timing and animated fill/type
agent-browser record start ./demo.webm --record-mode demo

# Explicit presentation effects
agent-browser record overlay text "Choose a plan" --position bottom
agent-browser record zoom to @e4 --scale 1.45

# Disable synthetic effects for a clean recording
agent-browser record start ./demo.webm --record-effects off

# Pick a different theme
agent-browser record start ./demo.webm --cursor dot

# Tune the animation (--cursor not required when only tuning)
agent-browser record start ./demo.webm \
  --cursor-tween-ms 350 --cursor-click-ms 500 --cursor-size 36

# Discard a bad take instead of saving it
agent-browser record abort
```

### Cursor Flags

| Flag                       | Default | Description                                                |
|----------------------------|---------|------------------------------------------------------------|
| `--record-effects <preset>` | cursor | Legacy preset alias: `cursor`, `demo`, or `off`. |
| `--record-mode <mode>`     | automation | `automation` or `demo`. `demo` keeps the cursor visible, uses slower cursor timing, blocks click timing, animates fill/type, and skips idle time between actions. |
| `--no-cursor`              | (off)   | Compatibility alias for `--record-effects off`. Cannot be combined with `--cursor`. |
| `--cursor <theme>`         | `arrow` | `arrow`, `dot`, `hand`, or `off`. Override the default theme or hide the cursor while keeping explicit effects available. |
| `--cursor-tween-ms <n>`    | 250     | Duration of the cursor's animated path between targets. Demo mode defaults to 700 unless explicitly set. |
| `--cursor-click-ms <n>`    | 400     | Duration of the click-ripple animation. Demo mode defaults to 500 unless explicitly set. |
| `--cursor-size <n>`        | 28      | Cursor size in CSS pixels (8-96).                          |
| `--cursor-motion <mode>`   | auto    | `auto` renders while effects are active, `always` keeps the idle cursor visible, and `off` disables tween motion entirely (cursor teleports). |
| `--cursor-block-clicks`    | off     | Await the tween before each click. Default is fire-and-forget so click latency is unchanged. |
| `--click-sync <mode>`      | async   | `async` or `block`. Replaces `--cursor-block-clicks` for new scripts. |
| `--input-mode <mode>`      | fast    | `fast` or `animated`. `demo` defaults to animated. |
| `--input-delay-ms <n>`     | 35 in animated mode | Per-character delay for animated fill/type. |

### Presentation Commands

```bash
agent-browser record overlay text "Explain this step" --position bottom
agent-browser record overlay spotlight @e4 --duration-ms 1200
agent-browser record overlay spotlight --x 640 --y 360 --radius 96 --duration-ms 1200
agent-browser record overlay clear
agent-browser record zoom to @e4 --scale 1.45
agent-browser record zoom to --x 640 --y 360 --scale 1.45
agent-browser record zoom reset
```

Spotlight and zoom accept either a selector/ref target or explicit `--x`/`--y` viewport coordinates. Text overlays serialize, stay visible for `--duration-ms`, and auto-dismiss before the next command continues. Spotlight holds for `--duration-ms`; selector targets derive radius from the element box, and any target can pass `--radius` to override it. Zoom holds until `record zoom reset`; pass `--duration-ms` for a temporary zoom. `record zoom to` waits for the camera transition, and `record zoom reset` waits for the reset transition, so compact command batches naturally capture the zoom animation. Use `record stop` to save and `record abort` to discard.

## Sound Effects

Effect-enabled recordings add bundled click sounds and typing sounds for animated input. `record stop` builds the interaction soundtrack on the same frame clock as the recording, then copies the Chromium-rendered video stream into the final file without re-encoding it. Page audio, narration, and system audio are not captured. `--record-effects off` and `--no-cursor` also disable the interaction soundtrack.

### Sync Model

By default the tween fires in parallel with the click (no added click latency). At 30 fps capture, a 250 ms tween shows up across multiple frames so the cursor visibly travels and lands as the click registers. When strict visual fidelity matters more than click timing, pass `--click-sync block` or use `--record-mode demo`.

The `demo` mode keeps page behavior unchanged except for intentional action timing: cursor flight and click pulses are slower, clicks wait for the cursor tween, fill/type use animated input defaults, and capture pauses between visual actions. Spotlight, text overlay, cursor, ripple, and zoom are browser-rendered recording effects, so the video captures the same smooth animations Chromium paints on the page.

For compact demos, open and settle the page before recording when initial load is not part of the story. Demo mode captures only visual activity and pauses between tool calls, so agent inference time and bare `wait` commands do not pad the video. Use overlay durations, zoom durations, and normal action effects to control what remains visible in the final clip.

### Limits

- **Recording-only.** The dashboard live screencast is unchanged.
- **Page-injected effects.** Effect-enabled recordings install a temporary recording layer into the captured page. It is removed when recording stops.
- **Frame coordinates.** Element centers are resolved through the active frame, then sent to the recording layer. Complex transformed iframe layouts may need verification in the final artifact.
- **`record restart` starts a new effects timeline.** Any active overlay or zoom is cleared when recording restarts.
- **Interaction audio only.** Recording does not capture page audio, narration, or system audio.

## Use Cases

### Debugging Failed Automation

```bash
#!/bin/bash
# Record automation for debugging

# Run your automation
agent-browser open https://app.example.com
agent-browser record start ./debug-$(date +%Y%m%d-%H%M%S).webm
agent-browser snapshot -i
agent-browser click @e1 || {
    echo "Click failed - check recording"
    agent-browser record stop
    exit 1
}

agent-browser record stop
```

### Documentation Generation

```bash
#!/bin/bash
# Record workflow for documentation

agent-browser open https://app.example.com/login
agent-browser record start ./docs/how-to-login.webm
agent-browser wait 1000  # Pause for visibility

agent-browser snapshot -i
agent-browser fill @e1 "demo@example.com"
agent-browser wait 500

agent-browser fill @e2 "password"
agent-browser wait 500

agent-browser click @e3
agent-browser wait --load networkidle
agent-browser wait 1000  # Show result

agent-browser record stop
```

### CI/CD Test Evidence

```bash
#!/bin/bash
# Record E2E test runs for CI artifacts

TEST_NAME="${1:-e2e-test}"
RECORDING_DIR="./test-recordings"
mkdir -p "$RECORDING_DIR"

agent-browser open
agent-browser record start "$RECORDING_DIR/$TEST_NAME-$(date +%s).webm"

# Run test
if run_e2e_test; then
    echo "Test passed"
else
    echo "Test failed - recording saved"
fi

agent-browser record stop
```

## Best Practices

### 1. Add Pauses for Clarity

```bash
# Slow down for human viewing
agent-browser click @e1
agent-browser wait 500  # Let viewer see result
```

### 2. Use Descriptive Filenames

```bash
# Include context in filename
agent-browser record start ./recordings/login-flow-2024-01-15.webm
agent-browser record start ./recordings/checkout-test-run-42.webm
```

### 3. Handle Recording in Error Cases

```bash
#!/bin/bash
set -e

cleanup() {
    agent-browser record stop 2>/dev/null || true
    agent-browser close 2>/dev/null || true
}
trap cleanup EXIT

agent-browser open
agent-browser record start ./automation.webm
# ... automation steps ...
```

### 4. Combine with Screenshots

```bash
# Record video AND capture key frames
agent-browser open https://example.com
agent-browser record start ./flow.webm
agent-browser screenshot ./screenshots/step1-homepage.png

agent-browser click @e1
agent-browser screenshot ./screenshots/step2-after-click.png

agent-browser record stop
```

## Output Format

- Default format: WebM (VP8/VP9 codec)
- Default frame rate: 30 fps (`--fps` accepts 1 to 60)
- Audio: click sounds and animated-input typing sounds when recording effects are enabled
- Compatible with all modern browsers and video players
- Compressed but high quality

## Limitations

- Recording adds slight overhead to automation, and higher frame rates add more
- Large recordings can consume significant disk space; 60 fps roughly doubles the bitrate of 30 fps
- Distinct frames per second are bounded by how often the page repaints, so a page rendering below 60 fps records below it too
- Some headless environments may have codec limitations
