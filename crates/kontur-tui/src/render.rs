use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};
use ratatui::Frame;

use crate::view::{ActiveRegion, KeyStatus, SessionView};

/// Draw the whole console. Pure: no I/O, no engine calls.
pub fn render(frame: &mut Frame, view: &SessionView) {
    let rows = Layout::vertical([
        Constraint::Length(1), // banner
        Constraint::Length(1), // status strip
        Constraint::Length(3), // stations
        Constraint::Length(5), // fleet
        Constraint::Min(3),    // log
        Constraint::Length(8), // active region
        Constraint::Length(1), // command line
    ])
    .split(frame.area());

    banner(frame, rows[0], view);
    status(frame, rows[1], view);
    stations(frame, rows[2], view);
    fleet(frame, rows[3], view);
    log(frame, rows[4], view);
    active(frame, rows[5], view);
    command(frame, rows[6]);
}

fn banner(frame: &mut Frame, area: Rect, view: &SessionView) {
    let text = format!(
        "[ КОНТУР-1  //  co-op session {}  //  v{} ]",
        view.banner.session, view.banner.version
    );
    frame.render_widget(
        Paragraph::new(text).style(Style::default().add_modifier(Modifier::BOLD)),
        area,
    );
}

fn status(frame: &mut Frame, area: Rect, view: &SessionView) {
    let s = &view.status;
    let needs = if s.needs_you > 0 {
        format!("FLEET {} ({} NEEDS YOU)", s.fleet_count, s.needs_you)
    } else {
        format!("FLEET {}", s.fleet_count)
    };
    let line = format!(
        " LINK {} || 4-EYES {} || {} || {} tok",
        if s.linked { "BOTH-STATIONS SYNC" } else { "B-STATION DROPPED" },
        if s.four_eyes { "ON" } else { "OFF" },
        needs,
        s.tokens
    );
    frame.render_widget(Paragraph::new(line), area);
}

fn stations(frame: &mut Frame, area: Rect, view: &SessionView) {
    let cols = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(area);
    for (i, st) in view.stations.iter().enumerate() {
        let block = Block::bordered().title(st.label.clone());
        let body = format!("{} · {}", st.role.label(), st.activity);
        frame.render_widget(Paragraph::new(body).block(block), cols[i]);
    }
}

fn fleet(frame: &mut Frame, area: Rect, view: &SessionView) {
    let lines: Vec<Line> = view
        .fleet
        .iter()
        .map(|a| {
            let marker = if a.needs_signoff { "▶ NEEDS SIGN-OFF" } else { a.status.as_str() };
            let text = format!(" {:<10} {:<20} {} tok", a.id, marker, a.tokens);
            if a.needs_signoff {
                Line::from(Span::styled(text, Style::default().add_modifier(Modifier::BOLD)))
            } else {
                Line::from(text)
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines).block(Block::bordered().title("FLEET")), area);
}

fn log(frame: &mut Frame, area: Rect, view: &SessionView) {
    let lines: Vec<Line> = view
        .log
        .iter()
        .map(|l| Line::from(format!(" {} {:<8} {}", l.time, l.who, l.text)))
        .collect();
    frame.render_widget(Paragraph::new(lines).block(Block::bordered().title("LOG")), area);
}

fn active(frame: &mut Frame, area: Rect, view: &SessionView) {
    match &view.active {
        ActiveRegion::Idle => {
            frame.render_widget(
                Paragraph::new(" no task dispatched — draft an instruction to begin")
                    .block(Block::bordered().title("PROMPT")),
                area,
            );
        }
        ActiveRegion::Prompt { prompt, ready } => {
            let a_mark = if ready[0] { "■" } else { "□" };
            let b_mark = if ready[1] { "■" } else { "□" };
            let lines = vec![
                Line::from(format!(" {}", prompt)),
                Line::from(format!(
                    " DISPATCH GATE   A ⟨{}⟩ ready   B ⟨{}⟩ ready",
                    a_mark, b_mark
                )),
                Line::from(" [y] mark ready — needs both"),
            ];
            frame.render_widget(
                Paragraph::new(lines).block(Block::bordered().title("PROMPT")),
                area,
            );
        }
        ActiveRegion::Plan { tasks, ready } => {
            let a_mark = if ready[0] { "■" } else { "□" };
            let b_mark = if ready[1] { "■" } else { "□" };
            let mut lines: Vec<Line> = tasks
                .iter()
                .map(|t| Line::from(format!(" t {}", t)))
                .collect();
            lines.push(Line::from(format!(
                " PLAN GATE   A ⟨{}⟩ ready   B ⟨{}⟩ ready",
                a_mark, b_mark
            )));
            lines.push(Line::from(" [y] approve plan — needs both"));
            frame.render_widget(
                Paragraph::new(lines).block(Block::bordered().title("PLAN")),
                area,
            );
        }
        ActiveRegion::Gate(card) => {
            let mut lines = vec![Line::from(format!(
                " GATE {} · {} · {} · +{} loc",
                card.gate_id,
                card.task,
                card.files.join(", "),
                card.loc
            ))];
            for key in &card.keys {
                let status = match key.status {
                    KeyStatus::Awaiting => "□ awaiting verdict",
                    KeyStatus::Sealed => "■ cast — sealed",
                    KeyStatus::Go => "■ GO",
                    KeyStatus::NoGo => "■ NO-GO",
                };
                lines.push(Line::from(format!("   KEY {:<12} {}", key.label, status)));
            }
            if card.escalation_required {
                lines.push(Line::from(Span::styled(
                    "   escalation required — co-signer must be a non-editor",
                    Style::default().add_modifier(Modifier::BOLD),
                )));
            }
            lines.push(Line::from(
                " [g] go   [r] no-go +remedy   [e] hand-edit   [o] open diff   [d] discuss",
            ));
            frame.render_widget(
                Paragraph::new(lines)
                    .block(Block::bordered().title("MERGE GATE"))
                    .wrap(Wrap { trim: true }),
                area,
            );
        }
        ActiveRegion::Intervention(card) => {
            let lines = vec![
                Line::from(format!(" NO-GO · {} — a remedy is required (steer or edit)", card.gate_id)),
                Line::from(format!(" steer > {}", card.steer)),
                Line::from(" [↵] send steer · [esc] cancel"),
            ];
            frame.render_widget(
                Paragraph::new(lines).block(Block::bordered().title("INTERVENTION")),
                area,
            );
        }
        ActiveRegion::SessionClosed(summary) => {
            let lines = vec![
                Line::from(format!(" {} gates", summary.gates)),
                Line::from(format!(" Reviewed-by: {}", summary.reviewers.join("   Reviewed-by: "))),
                Line::from(if summary.chain_verified {
                    " chain verified ✓ (tamper-evident)".to_string()
                } else {
                    " chain BROKEN ✗".to_string()
                }),
            ];
            frame.render_widget(
                Paragraph::new(lines).block(Block::bordered().title("SESSION COMPLETE")),
                area,
            );
        }
    }
}

fn command(frame: &mut Frame, area: Rect) {
    frame.render_widget(Paragraph::new(" > "), area);
}

/// Render a full-screen diff view with a close hint.
pub fn render_diff(frame: &mut Frame, title: &str, text: &str) {
    let block = Block::bordered().title(format!("{} — [o] close diff", title));
    frame.render_widget(
        Paragraph::new(text.to_owned())
            .block(block)
            .wrap(Wrap { trim: false }),
        frame.area(),
    );
}
