//! Agent clarification questions: a dual-consent Q&A the agent raises when the
//! prompt is ambiguous, resolved before it plans.
//!
//! Each question is multiple-choice with an implicit final "provide your own
//! answer" (free-text) option. Both operators answer every question. When their
//! answers to a question differ, that question re-asks with exactly three
//! options — the first operator's answer, the second's, or "accept both" — and
//! both must converge. There is no third party: converging is the operators' to
//! do (the same rule as gate disagreement).
//!
//! This module is the pure state machine — no I/O, no wire types. It is driven
//! by `answer()` and read by the projection accessors; the session server and
//! TUI layer on top.

use serde::{Deserialize, Serialize};

/// A question the agent asks. The "provide your own answer" option is implicit
/// — it is never stored in `options`; the UI always offers it as the last pick.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    pub prompt: String,
    pub options: Vec<String>,
}

/// One operator's pick for a question in the current round.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Choice {
    /// Index into the question's current options.
    Option(usize),
    /// The operator's own free-text answer ("provide your own").
    Custom(String),
}

impl Choice {
    /// Resolve to the answer text, given the options in effect for the round.
    fn text(&self, options: &[String]) -> String {
        match self {
            Choice::Option(i) => options.get(*i).cloned().unwrap_or_default(),
            Choice::Custom(s) => s.trim().to_owned(),
        }
    }
}

/// Per-question resolution state.
#[derive(Clone, Debug, PartialEq, Eq)]
enum QState {
    /// First round: collecting each seat's answer to the original question.
    Collecting {
        options: Vec<String>,
        a: Option<Choice>,
        b: Option<Choice>,
    },
    /// The two answers diverged; collecting reconciliation picks. The options
    /// in effect are `[a_text, b_text, "accept both"]`.
    Reconciling {
        a_text: String,
        b_text: String,
        a: Option<usize>,
        b: Option<usize>,
    },
    /// Done. One accepted answer, or two when the operators chose "accept both".
    Resolved(Vec<String>),
}

const ACCEPT_BOTH: &str = "accept both";

/// A dual-consent clarification exchange over a fixed set of questions.
#[derive(Clone, Debug)]
pub struct Clarify {
    prompts: Vec<String>,
    states: Vec<QState>,
}

impl Clarify {
    /// Start an exchange. Panics on zero questions — the agent must not call
    /// ask_clarification with an empty list (the server rejects that upstream).
    pub fn new(questions: Vec<Question>) -> Self {
        assert!(!questions.is_empty(), "clarification needs >= 1 question");
        let prompts = questions.iter().map(|q| q.prompt.clone()).collect();
        let states = questions
            .into_iter()
            .map(|q| QState::Collecting {
                options: q.options,
                a: None,
                b: None,
            })
            .collect();
        Clarify { prompts, states }
    }

    pub fn len(&self) -> usize {
        self.states.len()
    }

    pub fn is_empty(&self) -> bool {
        self.states.is_empty()
    }

    /// The options currently in effect for a question — the originals while
    /// collecting, or `[a_text, b_text, "accept both"]` while reconciling.
    /// Empty once resolved.
    pub fn options(&self, q: usize) -> Vec<String> {
        match self.states.get(q) {
            Some(QState::Collecting { options, .. }) => options.clone(),
            Some(QState::Reconciling { a_text, b_text, .. }) => {
                vec![a_text.clone(), b_text.clone(), ACCEPT_BOTH.to_owned()]
            }
            _ => Vec::new(),
        }
    }

    /// The prompt in effect for a question — the original while collecting, or a
    /// reconciliation prompt naming the divergence while reconciling.
    pub fn prompt(&self, q: usize) -> String {
        match self.states.get(q) {
            Some(QState::Reconciling { a_text, b_text, .. }) => format!(
                "operators differ on \"{}\" — A: {a_text} · B: {b_text} — agree which to use",
                self.prompts[q]
            ),
            _ => self.prompts.get(q).cloned().unwrap_or_default(),
        }
    }

    /// True when a seat is still owed an answer for this question.
    pub fn awaiting(&self, seat: usize, q: usize) -> bool {
        match self.states.get(q) {
            Some(QState::Collecting { a, b, .. }) => {
                (seat == 0 && a.is_none()) || (seat == 1 && b.is_none())
            }
            Some(QState::Reconciling { a, b, .. }) => {
                (seat == 0 && a.is_none()) || (seat == 1 && b.is_none())
            }
            _ => false,
        }
    }

    /// Whether custom free-text answers are offered for this question. Only in
    /// the first (Collecting) round — reconciliation is a fixed three-way pick.
    pub fn allows_custom(&self, q: usize) -> bool {
        matches!(self.states.get(q), Some(QState::Collecting { .. }))
    }

    /// Record a seat's answer to a question, advancing the state when both
    /// seats have answered. `seat` is 0 or 1. During reconciliation a `Custom`
    /// choice is coerced to an option index by matching text (the UI only
    /// offers indices there), falling back to a no-op on mismatch.
    pub fn answer(&mut self, seat: usize, q: usize, choice: Choice) {
        let Some(state) = self.states.get_mut(q) else {
            return;
        };
        // Compute the next state (if the answer completes a round) without
        // holding borrows into `state` across its reassignment.
        let next: Option<QState> = match state {
            QState::Collecting { options, a, b } => {
                if seat == 0 {
                    *a = Some(choice);
                } else {
                    *b = Some(choice);
                }
                match (a.as_ref(), b.as_ref()) {
                    (Some(ca), Some(cb)) => {
                        let ta = ca.text(options);
                        let tb = cb.text(options);
                        Some(if ta == tb {
                            QState::Resolved(vec![ta])
                        } else {
                            QState::Reconciling {
                                a_text: ta,
                                b_text: tb,
                                a: None,
                                b: None,
                            }
                        })
                    }
                    _ => None,
                }
            }
            QState::Reconciling {
                a_text,
                b_text,
                a,
                b,
            } => {
                let recon_opts = [a_text.clone(), b_text.clone(), ACCEPT_BOTH.to_owned()];
                let idx = match choice {
                    Choice::Option(i) if i < 3 => Some(i),
                    Choice::Custom(s) => recon_opts.iter().position(|o| *o == s.trim()),
                    _ => None,
                };
                let Some(idx) = idx else { return };
                if seat == 0 {
                    *a = Some(idx);
                } else {
                    *b = Some(idx);
                }
                match (*a, *b) {
                    (Some(ia), Some(ib)) if ia == ib => Some(QState::Resolved(match ia {
                        0 => vec![a_text.clone()],
                        1 => vec![b_text.clone()],
                        _ => vec![a_text.clone(), b_text.clone()],
                    })),
                    (Some(_), Some(_)) => {
                        // Still diverging — re-ask the same reconciliation.
                        *a = None;
                        *b = None;
                        None
                    }
                    _ => None,
                }
            }
            QState::Resolved(_) => None,
        };
        if let Some(next) = next {
            *state = next;
        }
    }

    /// True once every question is resolved.
    pub fn is_resolved(&self) -> bool {
        self.states.iter().all(|s| matches!(s, QState::Resolved(_)))
    }

    /// The resolved answers per original question (one or more strings each),
    /// or `None` while any question is still open.
    pub fn resolved(&self) -> Option<Vec<Vec<String>>> {
        self.is_resolved().then(|| {
            self.states
                .iter()
                .map(|s| match s {
                    QState::Resolved(v) => v.clone(),
                    _ => unreachable!("guarded by is_resolved"),
                })
                .collect()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(prompt: &str, options: &[&str]) -> Question {
        Question {
            prompt: prompt.into(),
            options: options.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn agreement_resolves_directly() {
        let mut c = Clarify::new(vec![q("target db?", &["postgres", "sqlite"])]);
        assert!(!c.is_resolved());
        c.answer(0, 0, Choice::Option(0));
        assert!(c.awaiting(1, 0));
        assert!(!c.awaiting(0, 0));
        c.answer(1, 0, Choice::Option(0));
        assert!(c.is_resolved());
        assert_eq!(c.resolved(), Some(vec![vec!["postgres".to_string()]]));
    }

    #[test]
    fn custom_answer_is_used() {
        let mut c = Clarify::new(vec![q("target db?", &["postgres", "sqlite"])]);
        c.answer(0, 0, Choice::Custom("  mysql  ".into()));
        c.answer(1, 0, Choice::Custom("mysql".into()));
        assert_eq!(c.resolved(), Some(vec![vec!["mysql".to_string()]]));
    }

    #[test]
    fn divergence_forces_reconciliation() {
        let mut c = Clarify::new(vec![q("target db?", &["postgres", "sqlite"])]);
        c.answer(0, 0, Choice::Option(0)); // postgres
        c.answer(1, 0, Choice::Option(1)); // sqlite
        assert!(!c.is_resolved());
        // Now reconciling: options are [postgres, sqlite, accept both].
        assert_eq!(c.options(0), vec!["postgres", "sqlite", "accept both"]);
        assert!(c.prompt(0).contains("operators differ"));
        assert!(!c.allows_custom(0));
        assert!(c.awaiting(0, 0) && c.awaiting(1, 0));

        // Both pick A's answer → resolved to postgres.
        c.answer(0, 0, Choice::Option(0));
        c.answer(1, 0, Choice::Option(0));
        assert_eq!(c.resolved(), Some(vec![vec!["postgres".to_string()]]));
    }

    #[test]
    fn accept_both_yields_two_answers() {
        let mut c = Clarify::new(vec![q("which linters?", &["clippy", "rustfmt"])]);
        c.answer(0, 0, Choice::Option(0));
        c.answer(1, 0, Choice::Option(1));
        // Reconcile option 2 = accept both.
        c.answer(0, 0, Choice::Option(2));
        c.answer(1, 0, Choice::Option(2));
        assert_eq!(
            c.resolved(),
            Some(vec![vec!["clippy".to_string(), "rustfmt".to_string()]])
        );
    }

    #[test]
    fn reconciliation_re_asks_on_continued_divergence() {
        let mut c = Clarify::new(vec![q("db?", &["pg", "sqlite"])]);
        c.answer(0, 0, Choice::Option(0));
        c.answer(1, 0, Choice::Option(1));
        // Reconcile but still disagree (A wants A's, B wants B's).
        c.answer(0, 0, Choice::Option(0));
        c.answer(1, 0, Choice::Option(1));
        assert!(!c.is_resolved(), "still diverging must re-ask, not resolve");
        assert!(c.awaiting(0, 0) && c.awaiting(1, 0));
        // Finally converge on accept-both.
        c.answer(0, 0, Choice::Option(2));
        c.answer(1, 0, Choice::Option(2));
        assert_eq!(
            c.resolved(),
            Some(vec![vec!["pg".to_string(), "sqlite".to_string()]])
        );
    }

    #[test]
    fn multiple_questions_resolve_independently() {
        let mut c = Clarify::new(vec![
            q("db?", &["pg", "sqlite"]),
            q("auth?", &["oauth", "basic"]),
        ]);
        // Q0 agrees immediately; Q1 diverges then reconciles.
        c.answer(0, 0, Choice::Option(0));
        c.answer(1, 0, Choice::Option(0));
        c.answer(0, 1, Choice::Option(0));
        c.answer(1, 1, Choice::Option(1));
        assert!(!c.is_resolved(), "Q1 still reconciling");
        c.answer(0, 1, Choice::Option(1));
        c.answer(1, 1, Choice::Option(1));
        assert_eq!(
            c.resolved(),
            Some(vec![vec!["pg".to_string()], vec!["basic".to_string()],])
        );
    }

    #[test]
    fn custom_texts_that_match_resolve_even_across_seats() {
        // A picks the "sqlite" option, B types "sqlite" — same text, resolved.
        let mut c = Clarify::new(vec![q("db?", &["pg", "sqlite"])]);
        c.answer(0, 0, Choice::Option(1));
        c.answer(1, 0, Choice::Custom("sqlite".into()));
        assert_eq!(c.resolved(), Some(vec![vec!["sqlite".to_string()]]));
    }
}
