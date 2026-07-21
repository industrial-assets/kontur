//! Boot screen: the КОНТУР identity card shown briefly on entry, before the
//! console appears. Brutalist — name, version, provenance; nothing animated.

use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::Frame;

/// How long the boot screen holds before the console takes over.
pub const BOOT_HOLD_MS: u64 = 1000;

/// Block glyphs spelling КОНТУР (Latin lookalikes K-O-H-T-Y-P).
const WORDMARK: [&str; 6] = [
    "██╗  ██╗ ██████╗ ██╗  ██╗████████╗██╗   ██╗██████╗ ",
    "██║ ██╔╝██╔═══██╗██║  ██║╚══██╔══╝╚██╗ ██╔╝██╔══██╗",
    "█████╔╝ ██║   ██║███████║   ██║    ╚████╔╝ ██████╔╝",
    "██╔═██╗ ██║   ██║██╔══██║   ██║     ╚██╔╝  ██╔═══╝ ",
    "██║  ██╗╚██████╔╝██║  ██║   ██║      ██║   ██║     ",
    "╚═╝  ╚═╝ ╚═════╝ ╚═╝  ╚═╝   ╚═╝      ╚═╝   ╚═╝     ",
];

/// The boot card's lines. Pure; tested.
pub fn boot_lines(version: &str) -> Vec<Line<'static>> {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let mut lines: Vec<Line<'static>> = WORDMARK
        .iter()
        .map(|row| Line::styled((*row).to_owned(), bold))
        .collect();
    lines.push(Line::raw(""));
    lines.push(Line::raw(format!(
        "КОНТУР-1 · v{version} · two keys, always"
    )));
    lines.push(Line::raw(""));
    lines.push(Line::raw(
        "© 2026 Industrial Assets · open source · no warranty",
    ));
    lines.push(Line::raw(
        "licence terms: github.com/industrial-assets/kontur",
    ));
    lines
}

/// Render the boot card centred in the full frame.
pub fn render_boot(frame: &mut Frame, version: &str) {
    let lines = boot_lines(version);
    let height = lines.len() as u16;
    let width = lines.iter().map(Line::width).max().unwrap_or(0) as u16;
    let area = centred(frame.area(), width, height);
    frame.render_widget(Paragraph::new(lines).centered(), area);
}

fn centred(area: Rect, width: u16, height: u16) -> Rect {
    let [area] = Layout::horizontal([Constraint::Length(width)])
        .flex(Flex::Center)
        .areas(area);
    let [area] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(area);
    area
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_lines_carry_identity_version_and_legal() {
        let lines = boot_lines("0.1.0");
        let text: String = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
                    + "\n"
            })
            .collect();
        // Wordmark rows present (first row of the block glyphs).
        assert!(text.contains("██╗  ██╗ ██████╗"));
        assert!(text.contains("КОНТУР-1 · v0.1.0"));
        assert!(text.contains("© 2026 Industrial Assets"));
        assert!(text.contains("open source"));
        assert!(text.contains("github.com/industrial-assets/kontur"));
    }

    #[test]
    fn wordmark_rows_are_equal_width() {
        let widths: Vec<usize> = WORDMARK
            .iter()
            .map(|r| r.chars().filter(|c| *c != '\u{fe0f}').count())
            .collect();
        assert!(
            widths.iter().all(|w| *w == widths[0]),
            "wordmark rows must align: {widths:?}"
        );
    }

    #[test]
    fn centred_clamps_within_area() {
        let r = centred(Rect::new(0, 0, 80, 24), 51, 11);
        assert!(r.x + r.width <= 80);
        assert!(r.y + r.height <= 24);
    }
}
