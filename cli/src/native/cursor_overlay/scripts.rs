//! Page-side scripts and asset compilation for the synthetic cursor overlay.
//!
//! The controller body lives in `cursor.js` and is `include_str!`d at compile
//! time. Each theme is a static SVG file whose contents are embedded into the
//! generated install script as a JSON-escaped string assigned to
//! `window.__ab_cursor_config.svg` before the controller runs.
//!
//! No filesystem I/O at runtime: every byte is part of the binary.

use serde::Serialize;

const CONTROLLER_JS: &str = include_str!("cursor.js");

const ARROW_SVG: &str = include_str!("themes/arrow.svg");
const DOT_SVG: &str = include_str!("themes/dot.svg");
const HAND_SVG: &str = include_str!("themes/hand.svg");

/// Built-in cursor themes. v1 ships three; user-supplied SVGs are deferred.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Theme {
    #[default]
    Arrow,
    Dot,
    Hand,
}

impl Theme {
    /// Stable string identifier used by the CLI parser and the JS config.
    pub fn as_str(self) -> &'static str {
        match self {
            Theme::Arrow => "arrow",
            Theme::Dot => "dot",
            Theme::Hand => "hand",
        }
    }

    /// SVG markup to inline into the cursor element via innerHTML.
    pub fn svg(self) -> &'static str {
        match self {
            Theme::Arrow => ARROW_SVG,
            Theme::Dot => DOT_SVG,
            Theme::Hand => HAND_SVG,
        }
    }

    /// Parse a theme name. Empty string returns the default theme.
    /// Case-insensitive.
    pub fn from_str_ci(s: &str) -> Result<Theme, String> {
        if s.is_empty() {
            return Ok(Theme::default());
        }
        match s.to_ascii_lowercase().as_str() {
            "arrow" => Ok(Theme::Arrow),
            "dot" => Ok(Theme::Dot),
            "hand" => Ok(Theme::Hand),
            other => Err(format!(
                "unknown cursor theme '{}'; valid options: arrow, dot, hand",
                other
            )),
        }
    }
}

/// Reduced-motion handling for the cursor tween.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MotionMode {
    /// Honor the host's `prefers-reduced-motion` setting (default).
    #[default]
    Auto,
    /// Always tween, regardless of reduced-motion preference.
    Always,
    /// Never tween. Cursor teleports.
    Off,
}

impl MotionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            MotionMode::Auto => "auto",
            MotionMode::Always => "always",
            MotionMode::Off => "off",
        }
    }

    pub fn from_str_ci(s: &str) -> Result<MotionMode, String> {
        if s.is_empty() {
            return Ok(MotionMode::default());
        }
        match s.to_ascii_lowercase().as_str() {
            "auto" => Ok(MotionMode::Auto),
            "always" => Ok(MotionMode::Always),
            "off" => Ok(MotionMode::Off),
            other => Err(format!(
                "unknown cursor motion mode '{}'; valid options: auto, always, off",
                other
            )),
        }
    }
}

/// Subset of `CursorOverlayConfig` that ships into the page. Kept private to
/// the JS templating path so the on-the-wire shape can evolve without
/// touching the public Rust API.
#[derive(Serialize)]
struct PageConfig<'a> {
    size: u32,
    #[serde(rename = "tweenMs")]
    tween_ms: u32,
    #[serde(rename = "clickMs")]
    click_ms: u32,
    motion: &'a str,
    svg: &'a str,
}

/// Build the install script: a `window.__ab_cursor_config = { ... };`
/// initializer followed by the controller body.
///
/// The controller is idempotent (Symbol-keyed install guard), so running this
/// script more than once on the same execution context is a no-op for the
/// second and later invocations.
pub fn build_install_script(
    theme: Theme,
    size: u32,
    tween_ms: u32,
    click_ms: u32,
    motion: MotionMode,
) -> String {
    let cfg = PageConfig {
        size,
        tween_ms,
        click_ms,
        motion: motion.as_str(),
        svg: theme.svg(),
    };
    // serde_json::to_string never fails for owned/borrowed primitive payloads
    // like this one. Fall back to a minimal empty config on the impossible
    // error path so the script remains syntactically valid.
    let cfg_json = serde_json::to_string(&cfg).unwrap_or_else(|_| "{}".to_string());
    let mut out = String::with_capacity(cfg_json.len() + CONTROLLER_JS.len() + 64);
    out.push_str("window.__ab_cursor_config = ");
    out.push_str(&cfg_json);
    out.push_str(";\n");
    out.push_str(CONTROLLER_JS);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_round_trip() {
        for theme in [Theme::Arrow, Theme::Dot, Theme::Hand] {
            assert_eq!(Theme::from_str_ci(theme.as_str()).unwrap(), theme);
        }
    }

    #[test]
    fn theme_parse_is_case_insensitive() {
        assert_eq!(Theme::from_str_ci("Arrow").unwrap(), Theme::Arrow);
        assert_eq!(Theme::from_str_ci("DOT").unwrap(), Theme::Dot);
    }

    #[test]
    fn theme_parse_empty_is_default() {
        assert_eq!(Theme::from_str_ci("").unwrap(), Theme::default());
        assert_eq!(Theme::default(), Theme::Arrow);
    }

    #[test]
    fn theme_parse_unknown_is_error() {
        let err = Theme::from_str_ci("rainbow").unwrap_err();
        assert!(err.contains("rainbow"));
        assert!(err.contains("arrow"));
        assert!(err.contains("dot"));
        assert!(err.contains("hand"));
    }

    #[test]
    fn motion_mode_round_trip() {
        for mode in [MotionMode::Auto, MotionMode::Always, MotionMode::Off] {
            assert_eq!(MotionMode::from_str_ci(mode.as_str()).unwrap(), mode);
        }
    }

    #[test]
    fn motion_mode_parse_unknown_is_error() {
        let err = MotionMode::from_str_ci("sometimes").unwrap_err();
        assert!(err.contains("sometimes"));
        assert!(err.contains("auto"));
        assert!(err.contains("always"));
        assert!(err.contains("off"));
    }

    #[test]
    fn build_install_script_contains_config_keys() {
        let script = build_install_script(Theme::Arrow, 28, 250, 150, MotionMode::Auto);
        assert!(script.contains("__ab_cursor_config"));
        assert!(script.contains("\"size\":28"));
        assert!(script.contains("\"tweenMs\":250"));
        assert!(script.contains("\"clickMs\":150"));
        assert!(script.contains("\"motion\":\"auto\""));
        assert!(script.contains("__ab_cursor_root__"));
    }

    #[test]
    fn build_install_script_inlines_chosen_theme_svg() {
        let arrow = build_install_script(Theme::Arrow, 28, 250, 150, MotionMode::Auto);
        let dot = build_install_script(Theme::Dot, 28, 250, 150, MotionMode::Auto);

        // Arrow has a path; dot uses circles. Use distinguishing markers.
        assert!(arrow.contains("<path"));
        assert!(dot.contains("<circle"));
        assert!(!arrow.contains("<circle"));
        assert!(!dot.contains("<path"));
    }

    #[test]
    fn build_install_script_with_zero_durations_is_valid_js_shape() {
        let script = build_install_script(Theme::Dot, 8, 0, 0, MotionMode::Off);
        assert!(script.contains("\"tweenMs\":0"));
        assert!(script.contains("\"clickMs\":0"));
        assert!(script.contains("\"motion\":\"off\""));
        // Smoke check: the JSON config prefix is well-formed by parsing the
        // first line back into a Value.
        let first_line = script.lines().next().unwrap();
        let stripped = first_line
            .strip_prefix("window.__ab_cursor_config = ")
            .unwrap()
            .trim_end_matches(';');
        let _: serde_json::Value = serde_json::from_str(stripped).expect("valid JSON config");
    }

    /// Anti-CSP regression: the controller and themes must never use any of
    /// the forms blocked by `style-src` (inline `<style>`, `setAttribute`'d
    /// style, or `cssText`). Programmatic per-property `el.style.x = ...`
    /// and constructable `CSSStyleSheet` are both fine and used heavily.
    #[test]
    fn install_script_avoids_csp_blocked_style_forms() {
        let script = build_install_script(Theme::Arrow, 28, 250, 150, MotionMode::Auto);

        // No inline <style>...</style> blocks. (SVGs may contain `style=`
        // attributes in user-supplied content, but our built-in themes
        // intentionally use only presentational attributes -- assert that.)
        assert!(
            !script.contains("<style"),
            "controller or themes leaked a <style> tag: triggers style-src"
        );
        assert!(
            !script.contains("cssText"),
            "controller used cssText: triggers style-src"
        );
        assert!(
            !script.contains("setAttribute('style'"),
            "controller used setAttribute('style', ...): triggers style-src"
        );
        assert!(
            !script.contains("setAttribute(\"style\""),
            "controller used setAttribute(\"style\", ...): triggers style-src"
        );
    }

    /// AGENTS.md forbids emoji glyphs in code/docs. Static check on the
    /// compiled-in JS and SVG payloads.
    #[test]
    fn install_script_contains_no_emoji_glyphs() {
        let script = build_install_script(Theme::Arrow, 28, 250, 150, MotionMode::Auto);
        for ch in script.chars() {
            let cp = ch as u32;
            // Emoji-heavy unicode blocks.
            let in_emoji_block = matches!(cp,
                0x1F300..=0x1FAFF
                | 0x2600..=0x27BF
                | 0x1F000..=0x1F2FF
            );
            assert!(
                !in_emoji_block,
                "install script contains emoji-block codepoint U+{:X}",
                cp
            );
        }
    }

    #[test]
    fn theme_svgs_are_non_empty_and_look_like_svg() {
        for theme in [Theme::Arrow, Theme::Dot, Theme::Hand] {
            let svg = theme.svg();
            assert!(
                svg.contains("<svg"),
                "theme {} missing <svg root",
                theme.as_str()
            );
            assert!(
                !svg.contains("<style"),
                "theme {} embedded a <style> tag (would trip style-src)",
                theme.as_str()
            );
            assert!(
                !svg.contains("<script"),
                "theme {} embedded a <script> tag",
                theme.as_str()
            );
        }
    }
}
