# Video Recording

Capture browser automation as video for debugging, documentation, or verification.

**Related**: [commands.md](commands.md) for full command reference, [SKILL.md](../SKILL.md) for quick start.

## Contents

- [Basic Recording](#basic-recording)
- [Recording Commands](#recording-commands)
- [Cursor Overlay](#cursor-overlay)
- [Use Cases](#use-cases)
- [Best Practices](#best-practices)
- [Output Format](#output-format)
- [Limitations](#limitations)

## Basic Recording

```bash
# Start recording
agent-browser record start ./demo.webm

# Perform actions
agent-browser open https://example.com
agent-browser snapshot -i
agent-browser click @e1
agent-browser fill @e2 "test input"

# Stop and save
agent-browser record stop
```

## Recording Commands

```bash
# Start recording to file
agent-browser record start ./output.webm

# Stop current recording
agent-browser record stop

# Restart with new file (stops current + starts new)
agent-browser record restart ./take2.webm
```

## Cursor Overlay

The OS cursor is never visible in `record start` output (CDP `Page.captureScreenshot`
renders the page DOM only, with no OS cursor). Pass `--cursor <theme>` to bake an
in-page synthetic cursor into the recording. The cursor tweens between targets and
pulses on click, captured directly into the WebM frames.

```bash
# Default cursor: arrow theme, 250ms tween, 28px size
agent-browser record start ./demo.webm https://example.com --cursor arrow

# Themes: arrow, dot, hand
agent-browser record start ./demo.webm --cursor dot

# Tune the animation
agent-browser record start ./demo.webm --cursor arrow \
  --cursor-tween-ms 350 --cursor-click-ms 200 --cursor-size 36
```

### Cursor Flags

| Flag                       | Default | Description                                                |
|----------------------------|---------|------------------------------------------------------------|
| `--cursor <theme>`         | (off)   | `arrow`, `dot`, or `hand`. Required to enable the overlay. |
| `--cursor-tween-ms <n>`    | 250     | Duration of the cursor's animated path between targets.    |
| `--cursor-click-ms <n>`    | 150     | Duration of the click-ripple animation.                    |
| `--cursor-size <n>`        | 28      | Cursor size in CSS pixels (8-96).                          |
| `--cursor-motion <mode>`   | auto    | `auto` honors the host's `prefers-reduced-motion`; `always` ignores it; `off` disables tween motion entirely (cursor teleports). |
| `--cursor-block-clicks`    | off     | Await the tween before each click. Default is fire-and-forget so click latency is unchanged. |

### Sync Model

By default the tween fires in parallel with the click (no added click latency).
At 10 fps capture, even a 250 ms tween shows up across 2-3 frames — visually
the cursor "flies in" and lands as the click registers. When strict visual
fidelity matters more than click timing, pass `--cursor-block-clicks` to await
the tween.

### Limits

- **Recording-only.** The dashboard live screencast is unchanged.
- **Chrome only.** `lightpanda` and other engines skip cursor install with an
  info-level log; the recording proceeds without a cursor.
- **Top-frame only.** Click coordinates inside iframes are correct, but the
  visual cursor is rendered in the top frame.
- **Skipped on mobile-emulation viewports** (touch input does not benefit from
  a synthetic cursor).
- **`record restart` re-uses the same browser session.** The previous cursor
  is removed before re-installing so a second cursor cannot accidentally
  appear.

## Use Cases

### Debugging Failed Automation

```bash
#!/bin/bash
# Record automation for debugging

agent-browser record start ./debug-$(date +%Y%m%d-%H%M%S).webm

# Run your automation
agent-browser open https://app.example.com
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

agent-browser record start ./docs/how-to-login.webm

agent-browser open https://app.example.com/login
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

agent-browser record start ./automation.webm
# ... automation steps ...
```

### 4. Combine with Screenshots

```bash
# Record video AND capture key frames
agent-browser record start ./flow.webm

agent-browser open https://example.com
agent-browser screenshot ./screenshots/step1-homepage.png

agent-browser click @e1
agent-browser screenshot ./screenshots/step2-after-click.png

agent-browser record stop
```

## Output Format

- Default format: WebM (VP8/VP9 codec)
- Compatible with all modern browsers and video players
- Compressed but high quality

## Limitations

- Recording adds slight overhead to automation
- Large recordings can consume significant disk space
- Some headless environments may have codec limitations
