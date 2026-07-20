use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};
use ratatui::Frame;

use crate::view::{ActiveRegion, KeyStatus, SessionView};

/// Draw the whole console. Pure: no I/O, no engine calls.
pub fn render(frame: &mut Frame, view: &SessionView) {
    // While the second station is unlinked, the invite is the one thing that
    // needs the human — it gets its own loud row and vanishes once linked.
    let invite_rows = match &view.invite {
        Some(text) => (text.lines().count() as u16) + 3, // lines + caveat + border
        None => 0,
    };
    let rows = Layout::vertical([
        Constraint::Length(1),           // banner
        Constraint::Length(1),           // status strip
        Constraint::Length(invite_rows), // invite (host, while unlinked)
        Constraint::Length(3),           // stations
        Constraint::Length(5),           // fleet
        Constraint::Min(3),              // log
        Constraint::Length(8),           // active region
        Constraint::Length(1),           // command line
    ])
    .split(frame.area());

    banner(frame, rows[0], view);
    status(frame, rows[1], view);
    if let Some(link) = &view.invite {
        invite(frame, rows[2], link);
    }
    stations(frame, rows[3], view);
    fleet(frame, rows[4], view);
    log(frame, rows[5], view);
    active(frame, rows[6], view);
    command(frame, rows[7], view);
}

fn invite(frame: &mut Frame, area: Rect, link: &str) {
    let mut lines: Vec<Line> = link
        .lines()
        .map(|l| {
            Line::from(Span::styled(
                format!(" {l}"),
                Style::default().add_modifier(Modifier::BOLD),
            ))
        })
        .collect();
    lines.push(Line::from(
        " send over a private channel — the link IS the operator's key",
    ));
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered().title("INVITE — OPERATOR NOT LINKED"))
            .wrap(Wrap { trim: false }),
        area,
    );
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
            if card.diff_opened {
                lines.push(Line::from(
                    " [g] go   [r] no-go +remedy   [e] hand-edit   [o] close diff   [d] discuss",
                ));
            } else {
                lines.push(Line::from(
                    " [o] open diff (required before go)   [r] no-go +remedy   [e] hand-edit   [d] discuss",
                ));
            }
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
            if summary.abandoned {
                let lines = vec![
                    Line::from(Span::styled(
                        " SESSION ABANDONED — nothing merged (audit chain intact)",
                        Style::default().add_modifier(Modifier::BOLD),
                    )),
                    Line::from(format!(" {} gates resolved before abandon", summary.gates)),
                    Line::from(if summary.chain_verified {
                        " chain verified ✓ (tamper-evident)".to_string()
                    } else {
                        " chain BROKEN ✗".to_string()
                    }),
                ];
                frame.render_widget(
                    Paragraph::new(lines).block(Block::bordered().title("SESSION ABANDONED")),
                    area,
                );
            } else {
                let mut lines = vec![
                    Line::from(format!(" {} gates", summary.gates)),
                    Line::from(format!(" Reviewed-by: {}", summary.reviewers.join("   Reviewed-by: "))),
                    Line::from(if summary.chain_verified {
                        " chain verified ✓ (tamper-evident)".to_string()
                    } else {
                        " chain BROKEN ✗".to_string()
                    }),
                ];
                if summary.merged {
                    lines.push(Line::from(" merged to repo ✓"));
                } else {
                    lines.push(Line::from(Span::styled(
                        " MERGE FAILED — work NOT landed in git (audit chain intact)",
                        Style::default().add_modifier(Modifier::BOLD),
                    )));
                }
                frame.render_widget(
                    Paragraph::new(lines).block(Block::bordered().title("SESSION COMPLETE")),
                    area,
                );
            }
        }
    }
}

fn command(frame: &mut Frame, area: Rect, view: &SessionView) {
    let text = match &view.notice {
        Some(msg) => {
            use ratatui::text::Line;
            let line = Line::from(Span::styled(
                format!(" > {msg}"),
                Style::default().add_modifier(Modifier::BOLD),
            ));
            Paragraph::new(line)
        }
        None => Paragraph::new(" > "),
    };
    frame.render_widget(text, area);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::{
        ActiveRegion, AuditSummary, Banner, Role, SessionView, Station,
        StatusStrip,
    };
    use kontur_core::OperatorId;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn minimal_view(active: ActiveRegion) -> SessionView {
        SessionView {
            banner: Banner { session: "test".into(), version: "0.0.0".into() },
            status: StatusStrip { linked: true, four_eyes: true, fleet_count: 0, needs_you: 0, tokens: 0 },
            stations: [
                Station { label: "A".into(), role: Role::Host, activity: "linked".into(), operator: OperatorId([1; 32]) },
                Station { label: "B".into(), role: Role::Operator, activity: "linked".into(), operator: OperatorId([2; 32]) },
            ],
            fleet: vec![],
            log: vec![],
            active,
            invite: None,
            notice: None,
        }
    }

    /// When merged=false the render must contain the loud failure notice.
    #[test]
    fn session_closed_merge_failed_renders_loud_notice() {
        let backend = TestBackend::new(120, 25);
        let mut terminal = Terminal::new(backend).unwrap();
        let view = minimal_view(ActiveRegion::SessionClosed(AuditSummary {
            gates: 1,
            reviewers: vec!["A".into(), "B".into()],
            chain_verified: true,
            merged: false,
            abandoned: false,
        }));
        terminal.draw(|f| render(f, &view)).unwrap();
        let rendered = terminal.backend().to_string();
        assert!(
            rendered.contains("MERGE FAILED"),
            "expected 'MERGE FAILED' in rendered output; got:\n{rendered}"
        );
        assert!(
            !rendered.contains("merged to repo"),
            "must not show success copy when merge failed"
        );
    }

    /// When abandoned=true the render must show SESSION ABANDONED loudly and
    /// must NOT show merged/Reviewed-by copy.
    #[test]
    fn session_abandoned_renders_loud() {
        let backend = TestBackend::new(120, 25);
        let mut terminal = Terminal::new(backend).unwrap();
        let view = minimal_view(ActiveRegion::SessionClosed(AuditSummary {
            gates: 2,
            reviewers: vec!["A".into(), "B".into()],
            chain_verified: true,
            merged: false,
            abandoned: true,
        }));
        terminal.draw(|f| render(f, &view)).unwrap();
        let rendered = terminal.backend().to_string();
        assert!(
            rendered.contains("SESSION ABANDONED"),
            "expected 'SESSION ABANDONED' in rendered output; got:\n{rendered}"
        );
        assert!(
            rendered.contains("nothing merged"),
            "expected 'nothing merged' in abandoned render; got:\n{rendered}"
        );
        assert!(
            !rendered.contains("merged to repo"),
            "must not show success copy when abandoned"
        );
        assert!(
            !rendered.contains("Reviewed-by"),
            "must not show Reviewed-by when abandoned"
        );
    }

    /// Golden test: notice=Some renders bold hint on the command row;
    /// notice=None renders the bare " > " prompt.
    #[test]
    fn command_row_renders_notice_when_some() {
        let backend = TestBackend::new(120, 25);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut view = minimal_view(ActiveRegion::Idle);
        view.notice = Some("open the diff first — [o]".into());
        terminal.draw(|f| render(f, &view)).unwrap();
        let rendered = terminal.backend().to_string();
        assert!(
            rendered.contains("open the diff first"),
            "expected notice text in rendered output; got:\n{rendered}"
        );
    }

    #[test]
    fn command_row_renders_bare_prompt_when_notice_none() {
        let backend = TestBackend::new(120, 25);
        let mut terminal = Terminal::new(backend).unwrap();
        let view = minimal_view(ActiveRegion::Idle);
        terminal.draw(|f| render(f, &view)).unwrap();
        let rendered = terminal.backend().to_string();
        assert!(
            rendered.contains(" > "),
            "expected bare prompt ' > ' in rendered output; got:\n{rendered}"
        );
    }

    /// When merged=true the render must show the success line.
    #[test]
    fn session_closed_merge_ok_renders_success_line() {
        let backend = TestBackend::new(120, 25);
        let mut terminal = Terminal::new(backend).unwrap();
        let view = minimal_view(ActiveRegion::SessionClosed(AuditSummary {
            gates: 2,
            reviewers: vec!["A".into(), "B".into()],
            chain_verified: true,
            merged: true,
            abandoned: false,
        }));
        terminal.draw(|f| render(f, &view)).unwrap();
        let rendered = terminal.backend().to_string();
        assert!(
            rendered.contains("merged to repo"),
            "expected 'merged to repo' in rendered output; got:\n{rendered}"
        );
        assert!(
            !rendered.contains("MERGE FAILED"),
            "must not show failure copy when merge succeeded"
        );
    }
}
