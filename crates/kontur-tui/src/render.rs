use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::diffview::styled_diff_lines;
use crate::view::{ActiveRegion, Attention, CursorTarget, KeyStatus, SessionView};

/// Draw the whole console. Pure: no I/O, no engine calls.
///
/// `diff_scroll` and `selected_file` are used when a Gate is the active region
/// (the diff is permanently on-screen in the right pane).
pub fn render(
    frame: &mut Frame,
    view: &SessionView,
    diff_scroll: u16,
    selected_file: usize,
    log_scroll: usize,
) {
    let invite_rows = match &view.invite {
        Some(text) => (text.lines().count() as u16) + 3,
        None => 0,
    };
    let attention_rows: u16 = if view.attention.is_some() { 1 } else { 0 };
    let agent_log_rows: u16 = if view.agent_log.is_some() { 1 } else { 0 };
    let rows = Layout::vertical([
        Constraint::Length(1),              // banner
        Constraint::Length(1),              // status strip
        Constraint::Length(attention_rows), // attention line (below status strip)
        Constraint::Length(invite_rows),    // invite (host, while unlinked)
        Constraint::Length(3),              // stations
        Constraint::Min(3),                 // panes (left + right)
        Constraint::Length(agent_log_rows), // host-only agent-log footer
        Constraint::Length(1),              // command line
    ])
    .split(frame.area());

    if view.link_lost {
        // A lost host outranks the identity flourish — the top line becomes a
        // loud alert until the link recovers or the operator quits.
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " HOST LOST — session frozen · casts will not land · [q] quit",
                Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED),
            ))),
            rows[0],
        );
    } else {
        banner(frame, rows[0], view);
    }
    status(frame, rows[1], view);
    if let Some(att) = &view.attention {
        attention_line(frame, rows[2], att);
    }
    if let Some(link) = &view.invite {
        invite(frame, rows[3], link);
    }
    stations(frame, rows[4], view);
    panes(frame, rows[5], view, diff_scroll, selected_file, log_scroll);
    if let Some(path) = &view.agent_log {
        agent_log_footer(frame, rows[6], path);
    }
    command(frame, rows[7], view);

    // Help overlay sits above everything else.
    if view.show_help {
        help_overlay(frame, view);
    }

    // Text-entry caret: drawn only on "on" frames of the blink cadence, so the
    // real terminal cursor flashes slowly at the insertion point.
    if view.blink_on {
        if let Some(pos) = caret_position(view, rows[5], rows[7]) {
            frame.set_cursor_position(pos);
        }
    }
}

/// Screen position for the text-entry caret, given the panes area and the
/// command row. `None` when not composing.
fn caret_position(view: &SessionView, panes_area: Rect, command_row: Rect) -> Option<(u16, u16)> {
    match view.cursor? {
        CursorTarget::Command { col } => {
            let x = command_row.x + col;
            let max_x = command_row.x + command_row.width.saturating_sub(1);
            Some((x.min(max_x), command_row.y))
        }
        CursorTarget::Prompt { index } => {
            // The PROMPT pane is the right 65% of the panes area; its text
            // renders inside a border with a one-space left margin.
            let cols = Layout::horizontal([Constraint::Percentage(35), Constraint::Percentage(65)])
                .split(panes_area);
            let pane = cols[1];
            // Caret line/column within the draft.
            let prompt = if let ActiveRegion::Prompt { prompt, .. } = &view.active {
                prompt.as_str()
            } else {
                return None;
            };
            let before: String = prompt.chars().take(index).collect();
            let line = before.matches('\n').count() as u16;
            let col = before
                .rsplit('\n')
                .next()
                .map(|l| l.chars().count())
                .unwrap_or(0) as u16;
            // +1 border, +1 leading space for x; +1 border for y.
            let x = pane.x + 2 + col;
            let y = pane.y + 1 + line;
            let max_x = pane.x + pane.width.saturating_sub(1);
            let max_y = pane.y + pane.height.saturating_sub(1);
            Some((x.min(max_x), y.min(max_y)))
        }
    }
}

/// The keymap lines for the current phase, plus the global keys. Pure; tested.
pub fn help_lines(active: &ActiveRegion, host_unlinked: bool) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    match active {
        ActiveRegion::Prompt { .. } => {
            out.push("PROMPT".into());
            out.push("  p    edit the instruction".into());
            out.push("  y    mark ready to dispatch (needs both)".into());
        }
        ActiveRegion::Clarify { .. } => {
            out.push("CLARIFY".into());
            out.push("  j/k  select question".into());
            out.push("  1-9  pick an option  a   provide your own".into());
            out.push("  both operators must answer; disagreements reconcile".into());
        }
        ActiveRegion::Plan { .. } => {
            out.push("PLAN REVIEW".into());
            out.push("  j/k  select task".into());
            out.push("  e    edit task     d   delete task".into());
            out.push("  < >  reorder task".into());
            out.push("  r    steer a replan (prompt the agent)".into());
            out.push("  y    approve the plan (needs both)".into());
        }
        ActiveRegion::Gate(_) => {
            out.push("MERGE GATE".into());
            out.push("  g    go            r   no-go + steer".into());
            out.push("  e    hand-edit a file  c   claim gate".into());
            out.push("  d    add a discuss note".into());
            out.push("  j/k  scroll diff   tab cycle file".into());
        }
        _ => {}
    }
    // When composing (any pane), the editor keys apply.
    out.push("COMPOSE (while typing)".into());
    out.push("  ↵    submit        esc cancel".into());
    out.push("  alt+↵ newline      ←/→/home/end move cursor".into());
    out.push("GLOBAL".into());
    out.push("  z    toggle AFK (away) · presence only, never merges alone".into());
    out.push("  ↑/↓  scroll log    K   abandon session".into());
    if host_unlinked {
        out.push("  l    invite: LAN / WAN".into());
    }
    out.push("  ?    close help    q   quit".into());
    out
}

fn help_overlay(frame: &mut Frame, view: &SessionView) {
    let host_unlinked = view.invite.is_some();
    let lines = help_lines(&view.active, host_unlinked);
    let width = lines
        .iter()
        .map(|l| l.chars().count())
        .max()
        .unwrap_or(20)
        .clamp(24, 60) as u16
        + 4;
    let height = lines.len() as u16 + 2;
    let area = centre(frame.area(), width, height);
    let body: Vec<Line> = lines
        .into_iter()
        .map(|l| {
            // Section headers (no leading space) are bold; key rows are calm.
            if l.starts_with(' ') {
                Line::from(l)
            } else {
                Line::from(Span::styled(
                    l,
                    Style::default().add_modifier(Modifier::BOLD),
                ))
            }
        })
        .collect();
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(body).block(Block::bordered().title("KEYS  [?] close")),
        area,
    );
}

/// Centre a `w`×`h` rect in `area` (clamped to it).
fn centre(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect::new(x, y, w, h)
}

fn attention_line(frame: &mut Frame, area: Rect, att: &Attention) {
    let style = if att.loud {
        Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)
    } else {
        Style::default().add_modifier(Modifier::DIM)
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(format!(" {}", att.text), style))),
        area,
    );
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
        " LINK {} || 4-EYES {} || {}",
        if s.linked {
            "BOTH-STATIONS SYNC"
        } else {
            "B-STATION DROPPED"
        },
        if s.four_eyes { "ON" } else { "OFF" },
        needs,
    );
    frame.render_widget(Paragraph::new(line), area);
}

fn stations(frame: &mut Frame, area: Rect, view: &SessionView) {
    let cols =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(area);
    for (i, st) in view.stations.iter().enumerate() {
        let block = Block::bordered().title(st.label.clone());
        // The station title already names the seat (e.g. "Operator A [Host]"),
        // so the body carries only the activity — no redundant role prefix.
        let body = if st.afk {
            "AFK".to_string()
        } else {
            st.activity.clone()
        };
        let p = if st.afk {
            Paragraph::new(Span::styled(
                body,
                Style::default().add_modifier(Modifier::DIM),
            ))
            .block(block)
        } else {
            Paragraph::new(body).block(block)
        };
        frame.render_widget(p, cols[i]);
    }
}

/// The dispatched instruction, shown above the fleet so the ask stays visible
/// through plan review and execution. First line only (wrapped); the full text
/// lived at the dispatch gate and is locked now.
fn task_bar(frame: &mut Frame, area: Rect, view: &SessionView) {
    let text = view
        .instruction
        .as_deref()
        .unwrap_or_default()
        .replace('\n', " ");
    frame.render_widget(
        Paragraph::new(Line::from(format!(" {text}")))
            .block(Block::bordered().title("TASK"))
            .wrap(Wrap { trim: true }),
        area,
    );
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
            let text = format!(" {:<10} {}", a.id, marker);
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

/// Visible window into the log: the last `height` entries, shifted back by
/// `scroll` (0 = stuck to the tail). Pure; tested.
pub fn log_window(len: usize, height: usize, scroll: usize) -> std::ops::Range<usize> {
    let scroll = scroll.min(len.saturating_sub(height));
    let end = len.saturating_sub(scroll);
    let start = end.saturating_sub(height);
    start..end
}

fn log(frame: &mut Frame, area: Rect, view: &SessionView, log_scroll: usize) {
    let height = area.height.saturating_sub(2) as usize; // borders
    let window = log_window(view.log.len(), height, log_scroll);
    let scrolled_back = window.end < view.log.len();
    let title = if scrolled_back {
        format!("LOG ↑{}", view.log.len() - window.end)
    } else {
        "LOG".to_owned()
    };
    let lines: Vec<Line> = view.log[window]
        .iter()
        .map(|l| {
            // Only pad columns that exist: server-formatted lines arrive with
            // empty time/who, and padding empties wastes ~11 leading columns.
            let mut s = String::from(" ");
            if !l.time.is_empty() {
                s.push_str(&l.time);
                s.push(' ');
            }
            if !l.who.is_empty() {
                s.push_str(&format!("{:<8} ", l.who));
            }
            s.push_str(&l.text);
            Line::from(s)
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(title)),
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
    log_scroll: usize,
) {
    let cols =
        Layout::horizontal([Constraint::Percentage(35), Constraint::Percentage(65)]).split(area);

    // Left pane: TASK line (when an instruction is live) + fleet + log.
    let task_rows: u16 = if view.instruction.is_some() { 3 } else { 0 };
    let left = Layout::vertical([
        Constraint::Length(task_rows),
        Constraint::Length(5),
        Constraint::Min(3),
    ])
    .split(cols[0]);
    if task_rows > 0 {
        task_bar(frame, left[0], view);
    }
    fleet(frame, left[1], view);
    log(frame, left[2], view, log_scroll);

    // Right pane: gate surface or phase card.
    match &view.active {
        ActiveRegion::Gate(card) => {
            // Rows: [files bar?] diff (flex) [discuss?] verdict bar. Files bar
            // drops below 15 rows to protect the diff; the discuss strip shows
            // only when the gate has notes and there's room (>= 18 rows).
            let files_height: u16 = if cols[1].height < 15 { 0 } else { 4 };
            let discuss_height: u16 = if card.discuss.is_empty() || cols[1].height < 18 {
                0
            } else {
                // up to 3 notes + 2 borders
                (card.discuss.len().min(3) as u16) + 2
            };

            let mut constraints = Vec::new();
            if files_height > 0 {
                constraints.push(Constraint::Length(files_height));
            }
            constraints.push(Constraint::Min(5)); // diff
            if discuss_height > 0 {
                constraints.push(Constraint::Length(discuss_height));
            }
            constraints.push(Constraint::Length(6)); // verdict bar

            let right = Layout::vertical(constraints).split(cols[1]);
            let mut i = 0;
            if files_height > 0 {
                render_files_bar(frame, right[i], card, selected_file);
                i += 1;
            }
            render_diff_pane(frame, right[i], card, diff_scroll, selected_file);
            i += 1;
            if discuss_height > 0 {
                render_discuss(frame, right[i], card);
                i += 1;
            }
            render_verdict_bar(frame, right[i], card);
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
    // Prefer the per-file diff sections (what [tab] actually cycles); fall
    // back to the recorded file list when no diff arrived.
    let names: Vec<&str> = if card.file_diffs.is_empty() {
        card.files.iter().map(String::as_str).collect()
    } else {
        card.file_diffs.iter().map(|fd| fd.path.as_str()).collect()
    };
    let files_str = names
        .iter()
        .enumerate()
        .map(|(i, f)| {
            if i == selected_file {
                format!("▶ {f}")
            } else {
                (*f).to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("  ");
    let claim = match &card.claimed_by {
        Some(who) => format!(" · ▸ {who} reviewing"),
        None => String::new(),
    };
    let lines = vec![
        Line::from(format!(" {} · +{} loc{}", files_str, card.loc, claim)),
        Line::from(" [tab] select · [e] edit · [c] claim"),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(format!("FILES — {}", card.gate_id))),
        area,
    );
}

fn render_diff_pane(
    frame: &mut Frame,
    area: Rect,
    card: &crate::view::GateCard,
    scroll: u16,
    selected_file: usize,
) {
    // Show only the tab-selected file's section; each file is truncated
    // independently, so the marker names the file it applies to.
    let selected = card
        .file_diffs
        .get(selected_file % card.file_diffs.len().max(1));
    let title = match selected {
        Some(fd) if fd.truncated => format!("DIFF — {} — {} (TRUNCATED)", card.gate_id, fd.path),
        Some(fd) => format!("DIFF — {} — {}", card.gate_id, fd.path),
        None => format!("DIFF — {}", card.gate_id),
    };
    let body: Vec<Line<'static>> = match selected {
        Some(fd) => styled_diff_lines(&fd.diff),
        None => vec![Line::from(" no diff available")],
    };
    frame.render_widget(
        Paragraph::new(body)
            .block(Block::bordered().title(title))
            .scroll((scroll, 0)),
        area,
    );
}

/// Gate-anchored discussion notes (last few), shown between diff and verdict.
fn render_discuss(frame: &mut Frame, area: Rect, card: &crate::view::GateCard) {
    let take = card.discuss.len().saturating_sub(3);
    let lines: Vec<Line> = card.discuss[take..]
        .iter()
        .map(|(who, text)| Line::from(format!(" {who}: {text}")))
        .collect();
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered().title("DISCUSS  [d] note"))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn render_verdict_bar(frame: &mut Frame, area: Rect, card: &crate::view::GateCard) {
    let mut lines: Vec<Line> = Vec::new();
    // Command outcome first — a failed run is the loudest fact on the card.
    if let Some((cmd, code)) = &card.last_cmd {
        let short: String = cmd.chars().take(48).collect();
        if *code == 0 {
            lines.push(Line::from(format!("   last cmd: {short} · exit 0")));
        } else {
            lines.push(Line::from(Span::styled(
                format!("   last cmd: {short} · FAILED exit {code}"),
                Style::default().add_modifier(Modifier::BOLD),
            )));
        }
    }
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
            // Multi-line instruction: each line of the draft renders; long
            // lines wrap rather than clip.
            let mut lines: Vec<Line> = prompt
                .split('\n')
                .map(|l| Line::from(format!(" {l}")))
                .collect();
            lines.push(Line::from(format!(
                " DISPATCH GATE   A ⟨{}⟩ ready   B ⟨{}⟩ ready",
                a_mark, b_mark
            )));
            lines.push(Line::from(" [p] edit prompt · [y] mark ready — needs both"));
            frame.render_widget(
                Paragraph::new(lines)
                    .block(Block::bordered().title("PROMPT"))
                    .wrap(Wrap { trim: false }),
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
        ActiveRegion::Clarify {
            questions,
            selected,
            own,
        } => {
            let mut lines: Vec<Line> = Vec::new();
            for (i, q) in questions.iter().enumerate() {
                let marker = if i == *selected { "▶" } else { " " };
                let status = match &q.resolved {
                    Some(ans) => format!("✓ {}", ans.join(" + ")),
                    None => {
                        let mine = q.picks[*own].as_deref();
                        let other = q.picks[1 - *own].as_deref();
                        match (mine, other) {
                            (Some(m), Some(o)) => format!("you: {m} · other: {o}"),
                            (Some(m), None) => format!("you: {m} · other: —"),
                            (None, Some(o)) => format!("you: — · other: {o}"),
                            (None, None) => "unanswered".into(),
                        }
                    }
                };
                lines.push(Line::from(format!(" {marker} Q{}: {}", i + 1, q.prompt)));
                for (oi, opt) in q.options.iter().enumerate() {
                    lines.push(Line::from(format!("      {}) {opt}", oi + 1)));
                }
                if q.allows_custom {
                    lines.push(Line::from(format!(
                        "      {}) provide your own — [a]",
                        q.options.len() + 1
                    )));
                }
                lines.push(Line::from(Span::styled(
                    format!("      · {status}"),
                    Style::default().add_modifier(Modifier::DIM),
                )));
            }
            lines.push(Line::from(
                " [j]/[k] question · [1-9] pick · [a] custom — both must answer",
            ));
            frame.render_widget(
                Paragraph::new(lines)
                    .block(Block::bordered().title("CLARIFY — agent needs answers"))
                    .wrap(Wrap { trim: true }),
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

/// Host-only dim footer naming the agent's session log, so the host can tail
/// the agent's narration without hunting for the path.
fn agent_log_footer(frame: &mut Frame, area: Rect, path: &str) {
    use ratatui::style::Color;
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" agent log: {path}"),
            Style::default().fg(Color::DarkGray),
        ))),
        area,
    );
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
            },
            stations: [
                Station {
                    label: "A".into(),
                    role: Role::Host,
                    activity: "linked".into(),
                    operator: OperatorId([1; 32]),
                    afk: false,
                },
                Station {
                    label: "B".into(),
                    role: Role::Operator,
                    activity: "linked".into(),
                    operator: OperatorId([2; 32]),
                    afk: false,
                },
            ],
            fleet: vec![],
            log: vec![],
            active,
            invite: None,
            notice: None,
            attention: None,
            instruction: None,
            show_help: false,
            agent_log: None,
            link_lost: false,
            cursor: None,
            blink_on: false,
        }
    }

    #[test]
    fn caret_command_column() {
        let mut v = minimal_view(ActiveRegion::Idle);
        v.cursor = Some(CursorTarget::Command { col: 17 });
        let cmd = ratatui::layout::Rect::new(0, 29, 120, 1);
        let panes = ratatui::layout::Rect::new(0, 5, 120, 20);
        assert_eq!(caret_position(&v, panes, cmd), Some((17, 29)));
    }

    #[test]
    fn caret_prompt_line_and_column() {
        let mut v = minimal_view(ActiveRegion::Prompt {
            prompt: "fix parser\nthen tests".into(),
            ready: [false, false],
        });
        // caret after "then " on line 1 (index = 10 nl + 5 = 16 -> col 5, line 1).
        v.cursor = Some(CursorTarget::Prompt { index: 16 });
        let panes = ratatui::layout::Rect::new(0, 5, 120, 20);
        let cmd = ratatui::layout::Rect::new(0, 29, 120, 1);
        let (x, y) = caret_position(&v, panes, cmd).unwrap();
        // right pane x=42; +2 border/margin; col 5 -> x=49. y = 5 (pane top)
        // +1 border +1 line = 7.
        assert_eq!((x, y), (49, 7));
    }

    #[test]
    fn log_window_math() {
        // Tail-stuck: last `height` entries.
        assert_eq!(log_window(10, 4, 0), 6..10);
        // Scrolled back two: window shifts back, clamped at the top.
        assert_eq!(log_window(10, 4, 2), 4..8);
        assert_eq!(log_window(10, 4, 99), 0..4);
        // Fewer entries than rows: everything shows.
        assert_eq!(log_window(3, 10, 0), 0..3);
        assert_eq!(log_window(3, 10, 5), 0..3);
        assert_eq!(log_window(0, 5, 0), 0..0);
    }

    #[test]
    fn help_lines_are_phase_aware() {
        let plan = help_lines(
            &ActiveRegion::Plan {
                tasks: vec![],
                ready: [false, false],
                selected: 0,
            },
            false,
        );
        assert!(plan.iter().any(|l| l.contains("approve the plan")));
        assert!(plan.iter().any(|l| l.contains("steer a replan")));
        assert!(!plan.iter().any(|l| l.contains("hand-edit")));

        let prompt = help_lines(
            &ActiveRegion::Prompt {
                prompt: String::new(),
                ready: [false, false],
            },
            false,
        );
        assert!(prompt.iter().any(|l| l.contains("edit the instruction")));

        // Global keys and the close hint are always present.
        for lines in [&plan, &prompt] {
            assert!(lines.iter().any(|l| l.contains("scroll log")));
            assert!(lines.iter().any(|l| l.contains("close help")));
        }
        // The invite key only when the host is still unlinked.
        assert!(help_lines(&ActiveRegion::Idle, true)
            .iter()
            .any(|l| l.contains("invite")));
        assert!(!help_lines(&ActiveRegion::Idle, false)
            .iter()
            .any(|l| l.contains("invite")));
    }

    fn draw(view: &SessionView) -> String {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, view, 0, 0, 0)).unwrap();
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
            file_diffs: vec![crate::view::FileDiffView {
                path: "auth/session.rs".into(),
                diff: "diff --git a/auth/session.rs b/auth/session.rs\n+fn foo() {}".into(),
                truncated: false,
            }],
            diff_truncated: false,
            last_cmd: None,
            claimed_by: None,
            discuss: Vec::new(),
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
            file_diffs: vec![],
            diff_truncated: false,
            last_cmd: None,
            claimed_by: None,
            discuss: Vec::new(),
        };
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render(f, &minimal_view(ActiveRegion::Gate(card)), 0, 1, 0))
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
            file_diffs: vec![crate::view::FileDiffView {
                path: "big.rs".into(),
                diff: "diff --git a/big.rs b/big.rs\n+fn big() {}".into(),
                truncated: true,
            }],
            diff_truncated: true,
            last_cmd: None,
            claimed_by: None,
            discuss: Vec::new(),
        };
        let rendered = draw(&minimal_view(ActiveRegion::Gate(card)));
        assert!(
            rendered.contains("TRUNCATED"),
            "truncated diff must show TRUNCATED in title; got:\n{rendered}"
        );
    }
}
