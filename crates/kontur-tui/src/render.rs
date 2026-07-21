use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};
use ratatui::Frame;

use crate::diffview::styled_diff_lines;
use crate::view::{ActiveRegion, KeyStatus, SessionView};

/// Draw the whole console. Pure: no I/O, no engine calls.
///
/// `diff_scroll` and `selected_file` are used when a Gate is the active region
/// (the diff is permanently on-screen in the right pane).
pub fn render(frame: &mut Frame, view: &SessionView, diff_scroll: u16, selected_file: usize) {
    let invite_rows = match &view.invite {
        Some(text) => (text.lines().count() as u16) + 3,
        None => 0,
    };
    let rows = Layout::vertical([
        Constraint::Length(1),           // banner
        Constraint::Length(1),           // status strip
        Constraint::Length(invite_rows), // invite (host, while unlinked)
        Constraint::Length(3),           // stations
        Constraint::Min(3),              // panes (left + right)
        Constraint::Length(1),           // command line
    ])
    .split(frame.area());

    banner(frame, rows[0], view);
    status(frame, rows[1], view);
    if let Some(link) = &view.invite {
        invite(frame, rows[2], link);
    }
    stations(frame, rows[3], view);
    panes(frame, rows[4], view, diff_scroll, selected_file);
    command(frame, rows[5], view);
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
        if s.linked {
            "BOTH-STATIONS SYNC"
        } else {
            "B-STATION DROPPED"
        },
        if s.four_eyes { "ON" } else { "OFF" },
        needs,
        s.tokens
    );
    frame.render_widget(Paragraph::new(line), area);
}

fn stations(frame: &mut Frame, area: Rect, view: &SessionView) {
    let cols =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(area);
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
            let marker = if a.needs_signoff {
                "▶ NEEDS SIGN-OFF"
            } else {
                a.status.as_str()
            };
            let text = format!(" {:<10} {:<20} {} tok", a.id, marker, a.tokens);
            if a.needs_signoff {
                Line::from(Span::styled(
                    text,
                    Style::default().add_modifier(Modifier::BOLD),
                ))
            } else {
                Line::from(text)
            }
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title("FLEET")),
        area,
    );
}

fn log(frame: &mut Frame, area: Rect, view: &SessionView) {
    let lines: Vec<Line> = view
        .log
        .iter()
        .map(|l| Line::from(format!(" {} {:<8} {}", l.time, l.who, l.text)))
        .collect();
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title("LOG")),
        area,
    );
}

/// Render the two-pane area (below stations, above command line).
/// Left 35%: fleet + log. Right 65%: phase surface or gate diff.
fn panes(
    frame: &mut Frame,
    area: Rect,
    view: &SessionView,
    diff_scroll: u16,
    selected_file: usize,
) {
    let cols =
        Layout::horizontal([Constraint::Percentage(35), Constraint::Percentage(65)]).split(area);

    // Left pane: fleet (5 rows compact) + log (rest).
    let left = Layout::vertical([Constraint::Length(5), Constraint::Min(3)]).split(cols[0]);
    fleet(frame, left[0], view);
    log(frame, left[1], view);

    // Right pane: gate surface or phase card.
    match &view.active {
        ActiveRegion::Gate(card) => {
            // Files bar: Length(4), diff: Min(5), verdict bar: Length(6).
            // Guard: if right pane height < 15, skip files bar to preserve diff.
            let (files_height, diff_min) = if cols[1].height < 15 {
                (0, Constraint::Min(5))
            } else {
                (4, Constraint::Min(5))
            };

            let constraints = if files_height > 0 {
                vec![
                    Constraint::Length(files_height),
                    diff_min,
                    Constraint::Length(6),
                ]
            } else {
                vec![diff_min, Constraint::Length(6)]
            };

            let right = Layout::vertical(constraints).split(cols[1]);

            if files_height > 0 {
                render_files_bar(frame, right[0], card, selected_file);
                render_diff_pane(frame, right[1], card, diff_scroll);
                render_verdict_bar(frame, right[2], card);
            } else {
                // Files bar dropped: diff is right[0], verdict is right[1]
                render_diff_pane(frame, right[0], card, diff_scroll);
                render_verdict_bar(frame, right[1], card);
            }
        }
        other => {
            render_phase_card(frame, cols[1], other);
        }
    }
}

fn render_files_bar(
    frame: &mut Frame,
    area: Rect,
    card: &crate::view::GateCard,
    selected_file: usize,
) {
    let files_str = card
        .files
        .iter()
        .enumerate()
        .map(|(i, f)| {
            if i == selected_file {
                format!("▶ {f}")
            } else {
                f.clone()
            }
        })
        .collect::<Vec<_>>()
        .join("  ");
    let lines = vec![
        Line::from(format!(" {} · +{} loc", files_str, card.loc)),
        Line::from(" [tab] select · [e] edit"),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(format!("FILES — {}", card.gate_id))),
        area,
    );
}

fn render_diff_pane(frame: &mut Frame, area: Rect, card: &crate::view::GateCard, scroll: u16) {
    let title = if card.diff_truncated {
        format!("DIFF — {} (TRUNCATED)", card.gate_id)
    } else {
        format!("DIFF — {}", card.gate_id)
    };
    let body: Vec<Line<'static>> = match &card.diff_preview {
        Some(text) => styled_diff_lines(text),
        None => vec![Line::from(" no diff available")],
    };
    frame.render_widget(
        Paragraph::new(body)
            .block(Block::bordered().title(title))
            .scroll((scroll, 0)),
        area,
    );
}

fn render_verdict_bar(frame: &mut Frame, area: Rect, card: &crate::view::GateCard) {
    let mut lines: Vec<Line> = Vec::new();
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
        " [g] go · [r] no-go+steer · [e] edit · [K] abandon",
    ));
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered().title("VERDICT"))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn render_phase_card(frame: &mut Frame, area: Rect, active: &ActiveRegion) {
    match active {
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
                Line::from(" [p] edit prompt · [y] mark ready — needs both"),
            ];
            frame.render_widget(
                Paragraph::new(lines).block(Block::bordered().title("PROMPT")),
                area,
            );
        }
        ActiveRegion::Plan {
            tasks,
            ready,
            selected,
        } => {
            let a_mark = if ready[0] { "■" } else { "□" };
            let b_mark = if ready[1] { "■" } else { "□" };
            let mut lines: Vec<Line> = tasks
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    let marker = if i == *selected { "▶" } else { " " };
                    Line::from(format!(" {} t{} {}", marker, i + 1, t))
                })
                .collect();
            lines.push(Line::from(format!(
                " PLAN GATE   A ⟨{}⟩ ready   B ⟨{}⟩ ready",
                a_mark, b_mark
            )));
            lines.push(Line::from(
                " [r] steer replan · j/k select · e edit · d delete · </> move · [y] approve — needs both",
            ));
            frame.render_widget(
                Paragraph::new(lines).block(Block::bordered().title("PLAN")),
                area,
            );
        }
        ActiveRegion::Intervention(card) => {
            let lines = vec![
                Line::from(format!(
                    " NO-GO · {} — a remedy is required (steer or edit)",
                    card.gate_id
                )),
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
                    Line::from(format!(
                        " Reviewed-by: {}",
                        summary.reviewers.join("   Reviewed-by: ")
                    )),
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
        ActiveRegion::Gate(_) => {
            // Gate is handled in panes() directly; this arm is unreachable.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::{
        ActiveRegion, AuditSummary, Banner, GateCard, KeyStatus, KeyView, Role, SessionView,
        Station, StatusStrip,
    };
    use kontur_core::OperatorId;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn minimal_view(active: ActiveRegion) -> SessionView {
        SessionView {
            banner: Banner {
                session: "test".into(),
                version: "0.0.0".into(),
            },
            status: StatusStrip {
                linked: true,
                four_eyes: true,
                fleet_count: 0,
                needs_you: 0,
                tokens: 0,
            },
            stations: [
                Station {
                    label: "A".into(),
                    role: Role::Host,
                    activity: "linked".into(),
                    operator: OperatorId([1; 32]),
                },
                Station {
                    label: "B".into(),
                    role: Role::Operator,
                    activity: "linked".into(),
                    operator: OperatorId([2; 32]),
                },
            ],
            fleet: vec![],
            log: vec![],
            active,
            invite: None,
            notice: None,
        }
    }

    fn draw(view: &SessionView) -> String {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, view, 0, 0)).unwrap();
        terminal.backend().to_string()
    }

    /// When merged=false the render must contain the loud failure notice.
    #[test]
    fn session_closed_merge_failed_renders_loud_notice() {
        let view = minimal_view(ActiveRegion::SessionClosed(AuditSummary {
            gates: 1,
            reviewers: vec!["A".into(), "B".into()],
            chain_verified: true,
            merged: false,
            abandoned: false,
        }));
        let rendered = draw(&view);
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
        let view = minimal_view(ActiveRegion::SessionClosed(AuditSummary {
            gates: 2,
            reviewers: vec!["A".into(), "B".into()],
            chain_verified: true,
            merged: false,
            abandoned: true,
        }));
        let rendered = draw(&view);
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
        let mut view = minimal_view(ActiveRegion::Idle);
        view.notice = Some("check the diff first".into());
        let rendered = draw(&view);
        assert!(
            rendered.contains("check the diff first"),
            "expected notice text in rendered output; got:\n{rendered}"
        );
    }

    #[test]
    fn command_row_renders_bare_prompt_when_notice_none() {
        let view = minimal_view(ActiveRegion::Idle);
        let rendered = draw(&view);
        assert!(
            rendered.contains(" > "),
            "expected bare prompt ' > ' in rendered output; got:\n{rendered}"
        );
    }

    /// Prompt region must show "[p] edit prompt" hint.
    #[test]
    fn prompt_region_shows_edit_hint() {
        let view = minimal_view(ActiveRegion::Prompt {
            prompt: "do the thing".into(),
            ready: [false, false],
        });
        let rendered = draw(&view);
        assert!(
            rendered.contains("[p] edit prompt"),
            "expected '[p] edit prompt' in rendered Prompt region; got:\n{rendered}"
        );
        assert!(
            rendered.contains("[y] mark ready"),
            "expected '[y] mark ready' in rendered Prompt region; got:\n{rendered}"
        );
        assert!(
            rendered.contains("do the thing"),
            "expected prompt text in rendered output; got:\n{rendered}"
        );
    }

    /// The plan-review hint must lead with the steer key and bracket both
    /// gate keys ([r] steer, [y] approve).
    #[test]
    fn plan_review_hint_leads_with_steer() {
        let view = minimal_view(ActiveRegion::Plan {
            tasks: vec!["task one".into(), "task two".into()],
            ready: [false, false],
            selected: 0,
        });
        let rendered = draw(&view);
        assert!(
            rendered.contains("[r] steer replan"),
            "expected '[r] steer replan' in rendered Plan region; got:\n{rendered}"
        );
        assert!(
            rendered.contains("[y] approve"),
            "expected '[y] approve' in rendered Plan region; got:\n{rendered}"
        );
    }

    /// When merged=true the render must show the success line.
    #[test]
    fn session_closed_merge_ok_renders_success_line() {
        let view = minimal_view(ActiveRegion::SessionClosed(AuditSummary {
            gates: 2,
            reviewers: vec!["A".into(), "B".into()],
            chain_verified: true,
            merged: true,
            abandoned: false,
        }));
        let rendered = draw(&view);
        assert!(
            rendered.contains("merged to repo"),
            "expected 'merged to repo' in rendered output; got:\n{rendered}"
        );
        assert!(
            !rendered.contains("MERGE FAILED"),
            "must not show failure copy when merge succeeded"
        );
    }

    /// Gate: diff title visible, verdict bar keys visible.
    #[test]
    fn gate_shows_diff_title_and_verdict_bar() {
        let card = GateCard {
            gate_id: "gate-001".into(),
            task: "t1".into(),
            files: vec!["auth/session.rs".into()],
            loc: 47,
            keys: vec![
                KeyView {
                    label: "A".into(),
                    role: Role::Host,
                    status: KeyStatus::Awaiting,
                },
                KeyView {
                    label: "B".into(),
                    role: Role::Operator,
                    status: KeyStatus::Sealed,
                },
            ],
            escalation_required: false,
            diff_preview: Some(
                "diff --git a/auth/session.rs b/auth/session.rs\n+fn foo() {}".into(),
            ),
            diff_truncated: false,
        };
        let rendered = draw(&minimal_view(ActiveRegion::Gate(card)));
        // Left LOG title visible simultaneously with right DIFF title.
        assert!(
            rendered.contains("LOG"),
            "LOG title must appear in left pane; got:\n{rendered}"
        );
        assert!(
            rendered.contains("DIFF"),
            "DIFF title must appear in right pane; got:\n{rendered}"
        );
        // Verdict bar keys.
        assert!(
            rendered.contains("[g] go"),
            "verdict bar must show [g] go; got:\n{rendered}"
        );
        // Sealed key renders correctly.
        assert!(
            rendered.contains("cast — sealed"),
            "sealed key must show 'cast — sealed'; got:\n{rendered}"
        );
        // Sealed key must NOT reveal a value.
        assert!(
            !rendered.contains("■ GO"),
            "sealed key must not show GO; got:\n{rendered}"
        );
        assert!(
            !rendered.contains("■ NO-GO"),
            "sealed key must not show NO-GO; got:\n{rendered}"
        );
    }

    /// Gate: files bar shows ▶ selection marker and LOC count.
    #[test]
    fn gate_files_bar_shows_selection_marker_and_loc() {
        let card = GateCard {
            gate_id: "gate-002".into(),
            task: "t2".into(),
            files: vec!["a.rs".into(), "b.rs".into()],
            loc: 10,
            keys: vec![],
            escalation_required: false,
            diff_preview: None,
            diff_truncated: false,
        };
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render(f, &minimal_view(ActiveRegion::Gate(card)), 0, 1))
            .unwrap();
        let rendered = terminal.backend().to_string();
        assert!(
            rendered.contains("▶ b.rs"),
            "selected file must be marked with ▶; got:\n{rendered}"
        );
        assert!(
            rendered.contains("+10 loc"),
            "files bar must show LOC count; got:\n{rendered}"
        );
    }

    /// Gate: truncated diff shows (TRUNCATED) in title.
    #[test]
    fn gate_truncated_diff_shows_truncated_in_title() {
        let card = GateCard {
            gate_id: "gate-003".into(),
            task: "t3".into(),
            files: vec!["big.rs".into()],
            loc: 9999,
            keys: vec![],
            escalation_required: false,
            diff_preview: Some("diff --git a/big.rs b/big.rs\n+fn big() {}".into()),
            diff_truncated: true,
        };
        let rendered = draw(&minimal_view(ActiveRegion::Gate(card)));
        assert!(
            rendered.contains("TRUNCATED"),
            "truncated diff must show TRUNCATED in title; got:\n{rendered}"
        );
    }
}
