//! The session goal as the composer shows it, ported from
//! `lib/sessionGoalMetadata.ts`, `lib/sessionGoalPresentation.ts` and the
//! first-turn reminder of `lib/sessionGoalActions.ts`.
//!
//! The OpenChamber server's goal loop (`web/server/lib/session-goal`) owns
//! the goal: it rides `metadata.openchamber.goal` on every session record and
//! `session.updated`, and the client writes it only to create, edit, pause,
//! resume, complete or clear it. Nothing here is persisted locally.

/// `SESSION_GOAL_OBJECTIVE_CHAR_LIMIT`: the longest objective the audit reads.
pub const OBJECTIVE_CHAR_LIMIT: usize = 5000;

/// The dialog's default budget and its bounds (`SessionGoalDialog`).
pub const DEFAULT_TOKEN_BUDGET: u64 = 200_000;
pub const MIN_TOKEN_BUDGET: u64 = 1000;
pub const MAX_TOKEN_BUDGET: u64 = 100_000_000;
pub const TOKEN_BUDGET_STEP: u64 = 50_000;

/// Where a goal stands, as the server settles it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GoalStatus {
    /// The loop drives the session toward the objective.
    Active,
    /// The user paused it, or stopped the turn it was running.
    Paused,
    /// Settled because only the user can unblock it, or a check failed.
    Blocked,
    /// Settled because the goal used its token budget.
    BudgetLimited,
    /// The progress check found every part done.
    Complete,
}

impl GoalStatus {
    /// The wire value, `null` for anything the reference does not know.
    pub fn from_wire(value: &str) -> Option<Self> {
        Some(match value {
            "active" => Self::Active,
            "paused" => Self::Paused,
            "blocked" => Self::Blocked,
            "budgetLimited" => Self::BudgetLimited,
            "complete" => Self::Complete,
            _ => return None,
        })
    }

    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Blocked => "blocked",
            Self::BudgetLimited => "budgetLimited",
            Self::Complete => "complete",
        }
    }

    /// `sessionGoalStatusLabelKey`.
    pub fn label_key(self) -> &'static str {
        match self {
            Self::Active => "chat.goal.status.active",
            Self::Paused => "chat.goal.status.paused",
            Self::Blocked => "chat.goal.status.blocked",
            Self::BudgetLimited => "chat.goal.status.budgetLimited",
            Self::Complete => "chat.goal.status.complete",
        }
    }

    /// `sessionGoalStatusColor`, as a role the renderer maps onto its theme.
    pub fn tone(self) -> GoalTone {
        match self {
            Self::Active => GoalTone::Info,
            Self::Paused => GoalTone::Muted,
            Self::Blocked | Self::BudgetLimited => GoalTone::Warning,
            Self::Complete => GoalTone::Success,
        }
    }

    /// Whether a reason is worth showing next to the status: only the
    /// failure states carry one (`SessionGoalDialog`).
    pub fn shows_reason(self) -> bool {
        matches!(self, Self::Blocked | Self::BudgetLimited)
    }
}

/// The theme role a goal status paints with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GoalTone {
    Info,
    Muted,
    Warning,
    Success,
}

/// `metadata.openchamber.goal` after `getSessionGoal` validated it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionGoal {
    /// Opaque per-goal identity; the server drops writes for another one.
    pub id: String,
    /// The inline objective, empty when it lives in the objective file.
    pub objective: String,
    /// The objective text is the server-side file keyed by the session.
    pub objective_file: bool,
    pub status: GoalStatus,
    pub token_budget: Option<u64>,
    pub tokens_used: u64,
    pub turns_used: u64,
    pub status_reason: String,
    pub evaluation_provider_id: String,
    pub evaluation_model_id: String,
    pub created_at: i64,
    pub updated_at: i64,
}

impl SessionGoal {
    /// The inline objective, or `None` while the text is the server's file.
    pub fn inline_objective(&self) -> Option<&str> {
        (!self.objective_file).then_some(self.objective.as_str())
    }

    /// The checking model as the dialog names it, `provider/model`.
    pub fn evaluation_model(&self) -> Option<String> {
        let parts: Vec<&str> = [&self.evaluation_provider_id, &self.evaluation_model_id]
            .into_iter()
            .map(String::as_str)
            .filter(|part| !part.is_empty())
            .collect();
        (!parts.is_empty()).then(|| parts.join("/"))
    }

    /// The row's Pause or Resume, by status (`SessionGoalRow`).
    pub fn row_action(&self) -> Option<GoalRowAction> {
        match self.status {
            GoalStatus::Active => Some(GoalRowAction::Pause),
            GoalStatus::Paused | GoalStatus::Blocked | GoalStatus::BudgetLimited => {
                Some(GoalRowAction::Resume)
            }
            GoalStatus::Complete => None,
        }
    }

    /// A goal that still drives the loop; a completed one turns the composer
    /// button back into an arm switch.
    pub fn is_live(&self) -> bool {
        self.status != GoalStatus::Complete
    }
}

/// The row's inline action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GoalRowAction {
    Pause,
    Resume,
}

impl GoalRowAction {
    pub fn label_key(self) -> &'static str {
        match self {
            Self::Pause => "chat.goal.action.pause",
            Self::Resume => "chat.goal.action.resume",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Self::Pause => "pause",
            Self::Resume => "play",
        }
    }
}

/// What the row shows as usage (`SessionGoalRow`): accounting lands only on
/// idle ticks, so without a budget the count hides until it is non-zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GoalUsage {
    Tokens { used: u64 },
    TokensWithBudget { used: u64, budget: u64 },
}

impl SessionGoal {
    /// The row's usage, `None` while there is nothing worth showing.
    pub fn row_usage(&self) -> Option<GoalUsage> {
        match self.token_budget {
            Some(budget) => Some(GoalUsage::TokensWithBudget {
                used: self.tokens_used,
                budget,
            }),
            None => (self.tokens_used > 0).then_some(GoalUsage::Tokens {
                used: self.tokens_used,
            }),
        }
    }

    /// The dialog's usage, shown even at zero.
    pub fn dialog_usage(&self) -> GoalUsage {
        match self.token_budget {
            Some(budget) => GoalUsage::TokensWithBudget {
                used: self.tokens_used,
                budget,
            },
            None => GoalUsage::Tokens {
                used: self.tokens_used,
            },
        }
    }
}

/// `formatGoalTokens`.
pub fn format_tokens(count: u64) -> String {
    if count >= 1_000_000 {
        format!("{}M", one_decimal(count as f64 / 1_000_000.0))
    } else if count >= 10_000 {
        format!("{}K", (count as f64 / 1000.0).round() as u64)
    } else if count >= 1_000 {
        format!("{}K", one_decimal(count as f64 / 1000.0))
    } else {
        count.to_string()
    }
}

/// `Number.prototype.toFixed(1)`, which breaks an exact tie upward where
/// Rust's formatter rounds it to even.
fn one_decimal(value: f64) -> String {
    format!("{:.1}", (value * 10.0).round() / 10.0)
}

/// The objective counted the way the reference counts it: JavaScript string
/// length, in UTF-16 code units.
pub fn objective_length(text: &str) -> usize {
    text.encode_utf16().count()
}

/// `TRIM_MARKER` of `fitObjective`.
const TRIM_MARKER: &str = "\n\n[… objective trimmed for the auditor — the full text was delivered in the chat message …]\n\n";

/// `fitObjective` without distillation: an objective over the limit keeps
/// its head and its tail around the marker, since the intent sits at the top
/// and the acceptance criteria at the bottom. Lengths are UTF-16 units.
pub fn fit_objective(raw: &str) -> String {
    let units: Vec<u16> = raw.encode_utf16().collect();
    if units.len() <= OBJECTIVE_CHAR_LIMIT {
        return raw.to_owned();
    }
    let half = OBJECTIVE_CHAR_LIMIT.saturating_sub(objective_length(TRIM_MARKER)) / 2;
    let head = String::from_utf16_lossy(&units[..half]);
    let tail = String::from_utf16_lossy(&units[units.len() - half..]);
    format!("{head}{TRIM_MARKER}{tail}")
}

/// `wrapSystemReminder`.
fn system_reminder(text: &str) -> String {
    format!("<system-reminder>\n{text}\n</system-reminder>")
}

/// `sessionGoalIntro`: the synthetic reminder an armed send carries, which
/// teaches the agent the goal protocol before the server's first
/// continuation. Same text as the server's mission and report instructions.
pub fn intro(token_budget: Option<u64>) -> String {
    let budget = token_budget
        .map(|budget| format!(" A token budget of {budget} tokens applies to this goal."))
        .unwrap_or_default();
    system_reminder(&format!(
        "Goal mode is active for this session. The user message above defines the goal objective.{budget}\n\n\
Work autonomously through the entire objective in this one run. The user is away and is not reading progress updates.\n\
- When a step is finished, start the next one in the same run.\n\
- Decide open questions yourself with the most sensible default, note the choice, and go on.\n\
- When something fails, find the cause and try another route.\n\
- Check each part by running it, and fix what the check finds.\n\
- Autonomy covers the objective's own work. Leave out irreversible or outward-facing actions it does not ask for.\n\
\nYour final message is read by an independent check, and it ends the run. Send it only when the whole objective is done and verified, or when only the user can unblock you. Stopping with work you could still do yourself is an early stop.\n\
\nBefore that message, stop every background command you started that does not finish on its own, such as a dev server or a watcher.\n\
\nEnd that message with a short status block, in the language of your reply, with exactly these four lines:\n\
- Done: what is finished.\n\
- Verified: how you checked it (the command, test or file), or \"not verified\".\n\
- Remaining: the parts of the objective you still have to do yourself, or \"nothing\".\n\
- Needs from the user: what only the user can provide before you can go on (credentials, access, a choice you cannot safely default), or \"nothing\".\n\
Unless the objective asks for them, testing in the app, review and committing are the user's steps after the work, not remaining work and not blockers. When it asks for them, such as a commit or a pull request, they are part of the work. Report what is true: unfinished work goes in Remaining."
    ))
}

/// Who checks a goal's progress (`sessionGoalChecker`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GoalChecker {
    /// The default: a classification provider set up for another feature is
    /// not consent to audit goals with Jev.
    #[default]
    SmallModel,
    Classifier,
}

/// `useGoalCheckAvailable`: false only when nothing can check a goal. Jev
/// checks when chosen and available; otherwise the small model must not be
/// known to be unavailable. An unknown answer allows the goal.
pub fn check_available(
    checker: GoalChecker,
    jev_available: bool,
    small_model: Option<bool>,
) -> bool {
    (checker == GoalChecker::Classifier && jev_available) || small_model != Some(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn goal(status: GoalStatus) -> SessionGoal {
        SessionGoal {
            id: "g".to_owned(),
            objective: "Ship it".to_owned(),
            objective_file: false,
            status,
            token_budget: None,
            tokens_used: 0,
            turns_used: 0,
            status_reason: String::new(),
            evaluation_provider_id: String::new(),
            evaluation_model_id: String::new(),
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn tokens_format_like_the_reference() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1_234), "1.2K");
        assert_eq!(format_tokens(9_999), "10.0K");
        assert_eq!(format_tokens(10_000), "10K");
        assert_eq!(format_tokens(199_600), "200K");
        assert_eq!(format_tokens(1_250_000), "1.3M");
    }

    #[test]
    fn the_row_offers_pause_while_active_and_resume_after_a_stop() {
        assert_eq!(
            goal(GoalStatus::Active).row_action(),
            Some(GoalRowAction::Pause)
        );
        for status in [
            GoalStatus::Paused,
            GoalStatus::Blocked,
            GoalStatus::BudgetLimited,
        ] {
            assert_eq!(goal(status).row_action(), Some(GoalRowAction::Resume));
        }
        assert_eq!(goal(GoalStatus::Complete).row_action(), None);
        assert!(!goal(GoalStatus::Complete).is_live());
    }

    #[test]
    fn usage_hides_a_zero_count_without_a_budget() {
        let mut unbudgeted = goal(GoalStatus::Active);
        assert_eq!(unbudgeted.row_usage(), None);
        assert_eq!(unbudgeted.dialog_usage(), GoalUsage::Tokens { used: 0 });
        unbudgeted.tokens_used = 12;
        assert_eq!(unbudgeted.row_usage(), Some(GoalUsage::Tokens { used: 12 }));
        let mut budgeted = goal(GoalStatus::Active);
        budgeted.token_budget = Some(1000);
        assert_eq!(
            budgeted.row_usage(),
            Some(GoalUsage::TokensWithBudget {
                used: 0,
                budget: 1000
            })
        );
    }

    #[test]
    fn statuses_round_trip_and_reject_unknown_values() {
        for status in [
            GoalStatus::Active,
            GoalStatus::Paused,
            GoalStatus::Blocked,
            GoalStatus::BudgetLimited,
            GoalStatus::Complete,
        ] {
            assert_eq!(GoalStatus::from_wire(status.as_wire()), Some(status));
        }
        assert_eq!(GoalStatus::from_wire("done"), None);
        assert!(GoalStatus::Blocked.shows_reason());
        assert!(!GoalStatus::Complete.shows_reason());
    }

    #[test]
    fn an_overlong_objective_keeps_head_and_tail() {
        let short = "a".repeat(OBJECTIVE_CHAR_LIMIT);
        assert_eq!(fit_objective(&short), short);
        let long = format!("{}{}", "h".repeat(4000), "t".repeat(4000));
        let fitted = fit_objective(&long);
        assert!(objective_length(&fitted) <= OBJECTIVE_CHAR_LIMIT);
        assert!(fitted.starts_with('h') && fitted.ends_with('t'));
        assert!(fitted.contains("objective trimmed for the auditor"));
    }

    #[test]
    fn the_intro_names_the_budget_only_when_set() {
        let plain = intro(None);
        assert!(plain.starts_with("<system-reminder>\nGoal mode is active"));
        assert!(plain.ends_with("unfinished work goes in Remaining.\n</system-reminder>"));
        assert!(!plain.contains("token budget"));
        assert!(intro(Some(5000)).contains(" A token budget of 5000 tokens applies to this goal."));
    }

    #[test]
    fn a_goal_can_start_unless_no_checker_can_run() {
        assert!(check_available(GoalChecker::SmallModel, false, None));
        assert!(check_available(GoalChecker::SmallModel, true, Some(true)));
        assert!(!check_available(GoalChecker::SmallModel, true, Some(false)));
        assert!(check_available(GoalChecker::Classifier, true, Some(false)));
        assert!(!check_available(
            GoalChecker::Classifier,
            false,
            Some(false)
        ));
    }

    #[test]
    fn the_evaluation_model_joins_what_is_set() {
        let mut checked = goal(GoalStatus::Active);
        assert_eq!(checked.evaluation_model(), None);
        checked.evaluation_model_id = "jev-1".to_owned();
        assert_eq!(checked.evaluation_model().as_deref(), Some("jev-1"));
        checked.evaluation_provider_id = "perf".to_owned();
        assert_eq!(checked.evaluation_model().as_deref(), Some("perf/jev-1"));
    }
}
