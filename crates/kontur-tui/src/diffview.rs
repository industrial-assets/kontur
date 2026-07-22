//! Diff viewer: parse unified-diff text into styled ratatui lines, file-list
//! extraction, scroll clamping, and a pure helper for $EDITOR selection.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

// ---------------------------------------------------------------------------
// Scroll state
// ---------------------------------------------------------------------------

/// Scroll position for the diff viewer.
pub struct DiffViewState {
    pub scroll: u16,
    pub total_lines: u16,
}

// ---------------------------------------------------------------------------
// Styling
// ---------------------------------------------------------------------------

/// Parse a unified-diff string into styled ratatui `Line`s (owned).
///
/// - `diff --git …` / `--- …` / `+++ …` lines → bold
/// - `@@ … @@` lines → cyan
/// - lines starting with `+` (but not `+++`) → green
/// - lines starting with `-` (but not `---`) → red
/// - everything else → plain (no extra colour)
pub fn styled_diff_lines(diff: &str) -> Vec<Line<'static>> {
    diff.lines()
        .map(|line| {
            let style = classify_line(line);
            Line::from(vec![Span::styled(line.to_owned(), style)])
        })
        .collect()
}

fn classify_line(line: &str) -> Style {
    if line.starts_with("diff --git")
        || line.starts_with("--- ")
        || line.starts_with("+++ ")
        || line.starts_with("index ")
        || line.starts_with("new file")
        || line.starts_with("deleted file")
    {
        // File-header lines: bold, no colour tint so they stay readable on any
        // terminal background.
        Style::default().add_modifier(Modifier::BOLD)
    } else if line.starts_with("@@") {
        Style::default().fg(Color::Cyan)
    } else if line.starts_with('+') {
        Style::default().fg(Color::Green)
    } else if line.starts_with('-') {
        Style::default().fg(Color::Red)
    } else {
        Style::default()
    }
}

// ---------------------------------------------------------------------------
// File extraction
// ---------------------------------------------------------------------------

/// Extract the file paths named in a unified diff.
///
/// Collects every `+++ b/<path>` line (new-file paths), deduplicating while
/// preserving order. `/dev/null` (removed files) is excluded.
pub fn diff_files(diff: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("+++ ") {
            // Strip the `b/` prefix that git adds.
            let path = rest.strip_prefix("b/").unwrap_or(rest);
            if path == "/dev/null" || path.is_empty() {
                continue;
            }
            if seen.insert(path.to_owned()) {
                out.push(path.to_owned());
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Scroll clamping
// ---------------------------------------------------------------------------

/// Clamp a scroll position to the valid range.
///
/// - Negative values → 0.
/// - Values beyond `max(0, total - viewport)` → that maximum.
///   (When the content fits in the viewport the max is 0.)
pub fn clamp_scroll(scroll: i32, total: u16, viewport: u16) -> u16 {
    if scroll <= 0 {
        return 0;
    }
    let max = total.saturating_sub(viewport);
    scroll.min(max as i32) as u16
}

// ---------------------------------------------------------------------------
// $EDITOR selection
// ---------------------------------------------------------------------------

/// Return the editor command to use for hand-edits.
///
/// Uses the value from the environment variable (if non-empty), otherwise
/// falls back to `"vi"`.
pub fn editor_command(env_val: Option<String>) -> String {
    match env_val {
        Some(v) if !v.trim().is_empty() => v,
        _ => "vi".to_owned(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Modifier};

    // -----------------------------------------------------------------------
    // styled_diff_lines
    // -----------------------------------------------------------------------

    fn sample_diff() -> &'static str {
        "diff --git a/src/lib.rs b/src/lib.rs\n\
         index abc123..def456 100644\n\
         --- a/src/lib.rs\n\
         +++ b/src/lib.rs\n\
         @@ -1,3 +1,4 @@\n\
          context line\n\
         -removed line\n\
         +added line\n\
          another context"
    }

    #[test]
    fn file_header_lines_are_bold() {
        let lines = styled_diff_lines(sample_diff());
        // "diff --git …" is the first line
        let diff_line = &lines[0];
        let span = &diff_line.spans[0];
        assert!(
            span.style.add_modifier.contains(Modifier::BOLD),
            "diff --git line should be bold; style={:?}",
            span.style
        );
        // "--- …" and "+++ …" lines should also be bold
        let minus_line = &lines[2]; // "--- a/src/lib.rs"
        assert!(
            minus_line.spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD),
            "--- line should be bold"
        );
        let plus_header = &lines[3]; // "+++ b/src/lib.rs"
        assert!(
            plus_header.spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD),
            "+++ line should be bold"
        );
    }

    #[test]
    fn hunk_header_is_cyan() {
        let lines = styled_diff_lines(sample_diff());
        let hunk = &lines[4]; // "@@ -1,3 +1,4 @@"
        let span = &hunk.spans[0];
        assert_eq!(
            span.style.fg,
            Some(Color::Cyan),
            "@@ line should be cyan; style={:?}",
            span.style
        );
    }

    #[test]
    fn addition_line_is_green() {
        let lines = styled_diff_lines(sample_diff());
        // "+added line" is at index 7
        let added = &lines[7];
        let span = &added.spans[0];
        assert_eq!(
            span.style.fg,
            Some(Color::Green),
            "+ line should be green; style={:?}",
            span.style
        );
    }

    #[test]
    fn deletion_line_is_red() {
        let lines = styled_diff_lines(sample_diff());
        // "-removed line" is at index 6
        let removed = &lines[6];
        let span = &removed.spans[0];
        assert_eq!(
            span.style.fg,
            Some(Color::Red),
            "- line should be red; style={:?}",
            span.style
        );
    }

    #[test]
    fn context_line_is_plain() {
        let lines = styled_diff_lines(sample_diff());
        // " context line" (space-prefixed) is at index 5
        let ctx = &lines[5];
        let span = &ctx.spans[0];
        assert_eq!(
            span.style.fg, None,
            "context line should have no foreground colour; style={:?}",
            span.style
        );
        assert!(
            !span.style.add_modifier.contains(Modifier::BOLD),
            "context line should not be bold"
        );
    }

    // -----------------------------------------------------------------------
    // diff_files
    // -----------------------------------------------------------------------

    #[test]
    fn extracts_new_file_path() {
        let diff = "+++ b/src/new_module.rs\n";
        assert_eq!(diff_files(diff), vec!["src/new_module.rs"]);
    }

    #[test]
    fn ignores_dev_null() {
        // A deleted file: +++ /dev/null should not appear in the list.
        let diff = "+++ /dev/null\n";
        assert!(diff_files(diff).is_empty(), "dev/null should be excluded");
    }

    #[test]
    fn deduplicates_same_file() {
        let diff = "+++ b/foo.rs\n+++ b/foo.rs\n";
        let files = diff_files(diff);
        assert_eq!(files, vec!["foo.rs"], "duplicates should be removed");
    }

    #[test]
    fn preserves_order_across_multiple_files() {
        let diff = "+++ b/alpha.rs\n+++ b/beta.rs\n+++ b/gamma.rs\n";
        let files = diff_files(diff);
        assert_eq!(files, vec!["alpha.rs", "beta.rs", "gamma.rs"]);
    }

    #[test]
    fn handles_path_without_b_prefix() {
        // e.g., non-git unified diffs use "+++ path/to/file"
        let diff = "+++ path/to/file.rs\n";
        let files = diff_files(diff);
        assert_eq!(files, vec!["path/to/file.rs"]);
    }

    // -----------------------------------------------------------------------
    // clamp_scroll
    // -----------------------------------------------------------------------

    #[test]
    fn negative_clamps_to_zero() {
        assert_eq!(clamp_scroll(-5, 100, 20), 0);
    }

    #[test]
    fn zero_stays_zero() {
        assert_eq!(clamp_scroll(0, 100, 20), 0);
    }

    #[test]
    fn valid_scroll_passes_through() {
        assert_eq!(clamp_scroll(10, 100, 20), 10);
    }

    #[test]
    fn past_end_clamps_to_max() {
        // total=100, viewport=20 → max=80
        assert_eq!(clamp_scroll(200, 100, 20), 80);
    }

    #[test]
    fn content_fits_in_viewport_max_is_zero() {
        // total=10, viewport=20 → max=0 (content fits)
        assert_eq!(clamp_scroll(5, 10, 20), 0);
    }

    #[test]
    fn small_viewport_keeps_the_bottom_reachable() {
        // A 15-line diff in a 12-row pane must scroll to its end (max = 15-12).
        // With the old fixed viewport of 20 this was max=0 — the bottom 3 lines
        // were cut off and unscrollable. Clamping with the real pane height fixes it.
        assert_eq!(clamp_scroll(999, 15, 12), 3);
        assert_eq!(clamp_scroll(999, 15, 20), 0); // the old, broken behaviour
    }

    // -----------------------------------------------------------------------
    // editor_command
    // -----------------------------------------------------------------------

    #[test]
    fn uses_env_val_when_set() {
        assert_eq!(editor_command(Some("nvim".to_owned())), "nvim");
    }

    #[test]
    fn falls_back_to_vi_when_none() {
        assert_eq!(editor_command(None), "vi");
    }

    #[test]
    fn falls_back_to_vi_when_empty() {
        assert_eq!(editor_command(Some(String::new())), "vi");
    }

    #[test]
    fn falls_back_to_vi_when_whitespace_only() {
        assert_eq!(editor_command(Some("   ".to_owned())), "vi");
    }
}
