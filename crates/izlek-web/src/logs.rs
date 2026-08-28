//! The admin's Logs screen: what mail is owed, what the rules decided, and
//! what happened across the workspace. Read-only — nothing here is written
//! back, so unlike Mail rules there is no action to guard beyond the read
//! itself.
//!
//! Ported from the old UI's `logs.rs`. That version read through a
//! server fn behind a `Resource`; here the page is rendered server-side on
//! every request, so `snapshot` — the shared read — backs both the page and
//! the JSON route `tests/http.rs` polls.

use serde::{Deserialize, Serialize};
use topcoat::Result;
use topcoat::context::Cx;
use topcoat::router::content::Json;
use topcoat::router::{page, route};
use topcoat::view::view;

use crate::detail::Me;
use crate::i18n::{Key, Lang, t};
use crate::server::{Refusal, accounts, require_admin};

/// One send still owed or refused, as the queue panel reads it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct QueueLine {
    recipient: String,
    subject: String,
    /// The localized word shown to the admin.
    state: String,
    /// "pending"/"held"/"failed"/"sent"/"abandoned" — the chip's tone, not
    /// its wording.
    state_kind: String,
    attempts: u32,
    last_error: Option<String>,
    next_attempt: Option<String>,
}

/// One rule's verdict inside a decision block.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct RuleVerdict {
    rule: String,
    outcome: String,
    /// `MailOutcome::as_str()` — the chip's tone, not its wording.
    outcome_kind: String,
    detail: String,
}

/// Every rule's verdict on one event, so the task and moment are said once
/// rather than repeated per rule.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct DecisionGroup {
    event_id: String,
    task: String,
    /// What the event did — "moved to Review" — when the event and its
    /// destination column are both still there; absent rather than guessed
    /// at otherwise.
    happened: Option<String>,
    at: String,
    verdicts: Vec<RuleVerdict>,
}

/// One line of the workspace activity feed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct ActivityRow {
    at: String,
    actor: String,
    sentence: String,
    title: String,
}

/// The screen in one answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct LogsSnapshot {
    me: Me,
    queue: Vec<QueueLine>,
    decisions: Vec<DecisionGroup>,
    activity: Vec<ActivityRow>,
}

/// How many mail is owed, what the rules decided, and what happened, all
/// capped so the screen stays a read a person can finish.
const LIMIT: u32 = 50;

/// The word the decisions panel prints for an outcome. Terse: the panel is
/// read, not explained.
fn outcome_word(outcome: izlek_core::store::MailOutcome, lang: Lang) -> &'static str {
    use izlek_core::store::MailOutcome;
    match outcome {
        MailOutcome::Owed => t(lang, Key::OutcomeQueued),
        MailOutcome::AlreadyOwed => t(lang, Key::OutcomeAlreadyQueued),
        MailOutcome::NoRecipients => t(lang, Key::OutcomeNoRecipients),
        MailOutcome::NotMatched => t(lang, Key::OutcomeNotMatched),
        MailOutcome::Disabled => t(lang, Key::OutcomeRuleOff),
        MailOutcome::TaskGone => t(lang, Key::OutcomeTaskGone),
    }
}

/// The sentence after a task's name, mirroring `ActivityEntry::sentence` —
/// the per-task strip on the detail screen — since the workspace feed carries
/// the same kind and detail but no `Person` to build an `ActivityEntry` from.
fn activity_sentence(kind: &izlek_core::detail::ActivityKind, detail: &str, lang: Lang) -> String {
    use izlek_core::detail::ActivityKind;
    let detail = detail.trim();
    match kind {
        ActivityKind::Created => t(lang, Key::ActCreated).to_string(),
        ActivityKind::Retitled => t(lang, Key::ActRetitled).to_string(),
        ActivityKind::Described => t(lang, Key::ActDescribed).to_string(),
        ActivityKind::DeadlineSet => crate::i18n::deadline_set_label(lang, detail),
        ActivityKind::DeadlineCleared => t(lang, Key::ActDeadlineCleared).to_string(),
        ActivityKind::Assigned => crate::i18n::assigned_label(lang, detail),
        ActivityKind::Unassigned => crate::i18n::unassigned_label(lang, detail),
        ActivityKind::Linked => crate::i18n::linked_label(lang, detail),
        ActivityKind::Unlinked => crate::i18n::unlinked_label(lang, detail),
        ActivityKind::Moved => crate::i18n::moved_label(lang, detail),
        ActivityKind::Unblocked => crate::i18n::unblocked_label(lang, detail),
        ActivityKind::Deleted => t(lang, Key::ActDeleted).to_string(),
        ActivityKind::Commented => t(lang, Key::ActCommented).to_string(),
        ActivityKind::Other(_) => detail.to_string(),
    }
}

/// What an event did, in the decisions panel's own words — "moved to
/// Review" — or nothing when the event or its destination column is gone:
/// the panel names what it can still read, never a guess.
async fn event_happened(
    store: &std::sync::Arc<dyn izlek_core::store::Store>,
    event_id: &str,
    lang: Lang,
) -> std::result::Result<Option<String>, izlek_core::store::StoreError> {
    use izlek_core::store::Event;

    let Some(event) = store.event(event_id).await? else {
        return Ok(None);
    };
    match event {
        Event::Moved(transition) => {
            let Some(board_id) = store.board_of_task(&transition.task_id).await? else {
                return Ok(None);
            };
            let columns = store.columns_for_board(&board_id).await?;
            Ok(columns
                .into_iter()
                .find(|column| column.id == transition.to_column)
                .map(|column| crate::i18n::moved_to_label(lang, &column.name)))
        }
        Event::Freed(_) => Ok(Some(t(lang, Key::UnblockedWord).to_string())),
        // Not a wired path yet — S4's work — so this only needs to be
        // exhaustive, not right.
        Event::Happened(activity) => Ok(Some(activity.kind.as_str().to_string())),
    }
}

/// A decision's `detail` column, rendered in the viewer's language.
///
/// New rows carry a machine token (see `store::MailDecision::detail`);
/// column references in it are IDs, resolved here against the rule's board,
/// one fetch per board no matter how many decisions share it. A detail that
/// is empty or does not parse as a known token — including every row a
/// version before this scheme wrote — is shown exactly as stored.
async fn decision_detail(
    store: &std::sync::Arc<dyn izlek_core::store::Store>,
    columns_cache: &mut std::collections::HashMap<String, Vec<izlek_core::board::Column>>,
    board_id: Option<&str>,
    outcome: izlek_core::store::MailOutcome,
    detail: &str,
    lang: Lang,
) -> std::result::Result<String, izlek_core::store::StoreError> {
    use izlek_core::store::MailOutcome;

    if detail.is_empty() || !matches!(outcome, MailOutcome::NoRecipients | MailOutcome::NotMatched)
    {
        return Ok(detail.to_string());
    }
    match (outcome, detail) {
        (MailOutcome::NoRecipients, "empty") => return Ok(t(lang, Key::AudienceEmpty).to_string()),
        (MailOutcome::NoRecipients, "actor_only") => {
            return Ok(t(lang, Key::AudienceActorOnly).to_string());
        }
        (MailOutcome::NotMatched, "not_status") => {
            return Ok(t(lang, Key::NotAStatusCrossing).to_string());
        }
        (MailOutcome::NotMatched, "not_unblocked") => {
            return Ok(t(lang, Key::NotAnUnblockedEvent).to_string());
        }
        _ => {}
    }
    if outcome != MailOutcome::NotMatched {
        return Ok(detail.to_string());
    }
    let Some(board_id) = board_id else {
        return Ok(detail.to_string());
    };
    let columns = match columns_cache.get(board_id) {
        Some(columns) => columns,
        None => {
            let columns = store.columns_for_board(board_id).await?;
            columns_cache.insert(board_id.to_string(), columns);
            columns_cache.get(board_id).expect("just inserted")
        }
    };
    let name = |id: &str| {
        columns
            .iter()
            .find(|column| column.id == id)
            .map(|column| column.name.clone())
            .unwrap_or_else(|| t(lang, Key::AColumn).to_string())
    };
    if let Some(rest) = detail.strip_prefix("moved:") {
        let mut parts = rest.splitn(2, ':');
        if let (Some(to), Some(watched)) = (parts.next(), parts.next()) {
            return Ok(crate::i18n::moved_not_watched_label(
                lang,
                &name(to),
                &name(watched),
            ));
        }
    }
    if let Some(watched) = detail.strip_prefix("unblocked:") {
        return Ok(crate::i18n::freed_not_watched_label(lang, &name(watched)));
    }
    if let Some(rest) = detail.strip_prefix("happened:") {
        let mut parts = rest.splitn(2, ':');
        let kind = parts.next().unwrap_or("");
        let watches = parts.next().unwrap_or("");
        let watched = if let Some(column) = watches.strip_prefix("status:") {
            crate::i18n::watches_move_phrase(lang, &name(column))
        } else if watches == "unblocked" {
            crate::i18n::watches_unblock_phrase(lang).to_string()
        } else if let Some(other_kind) = watches.strip_prefix("kind:") {
            crate::i18n::activity_kind_word(lang, other_kind)
        } else {
            return Ok(detail.to_string());
        };
        let event_word = crate::i18n::activity_kind_word(lang, kind);
        return Ok(crate::i18n::happened_not_watched_label(
            lang,
            &event_word,
            &watched,
        ));
    }
    Ok(detail.to_string())
}

/// The queue, the decisions, and the activity, admin only. Shared by the
/// `/logs` page and the `/api/current_logs` route so both answer the same
/// read the same way.
async fn snapshot(cx: &Cx) -> Result<std::result::Result<LogsSnapshot, Refusal>> {
    use izlek_core::store::SendState;

    let user = match require_admin(cx).await {
        Ok(user) => user,
        Err(refusal) => return Ok(Err(refusal)),
    };
    let lang = Lang::from_code(&user.language);
    let zone = izlek_core::detail::parse_zone(&user.timezone);
    let store = accounts(cx).store().clone();

    let sends = store.mail_queue(LIMIT).await?;
    let mut queue = Vec::with_capacity(sends.len());
    for send in sends {
        let subject = match &send.subject {
            Some(subject) => subject.clone(),
            None => match &send.rule_id {
                Some(id) => store
                    .mail_rule(id)
                    .await?
                    .map(|rule| rule.subject)
                    .unwrap_or_else(|| t(lang, Key::RuleGone).to_string()),
                None => t(lang, Key::RuleGone).to_string(),
            },
        };
        let (state, state_kind) = match send.state {
            SendState::Pending => (t(lang, Key::QueueStatePending), "pending"),
            // A send the engine never attempted is only held — usually
            // for want of a sender — not failed at anything.
            SendState::Failed if send.attempts == 0 => (t(lang, Key::QueueStateHeld), "held"),
            SendState::Failed => (t(lang, Key::QueueStateFailed), "failed"),
            SendState::Sent => (t(lang, Key::QueueStateSent), "sent"),
            SendState::Abandoned => (t(lang, Key::QueueStateAbandoned), "abandoned"),
        };
        queue.push(QueueLine {
            recipient: send.recipient,
            subject,
            state: state.to_string(),
            state_kind: state_kind.to_string(),
            attempts: send.attempts,
            last_error: send.last_error,
            next_attempt: send
                .next_attempt_at
                .map(|at| izlek_core::detail::moment_label_in(at, zone)),
        });
    }

    let raw_decisions = store.recent_mail_decisions(LIMIT).await?;
    let mut decisions: Vec<DecisionGroup> = Vec::new();
    let mut columns_cache: std::collections::HashMap<String, Vec<izlek_core::board::Column>> =
        std::collections::HashMap::new();
    for decision in raw_decisions {
        let rule = store.mail_rule(&decision.rule_id).await?;
        let rule_label = rule
            .as_ref()
            .map(|rule| rule.subject.clone())
            .unwrap_or_else(|| t(lang, Key::RuleGone).to_string());
        let detail = decision_detail(
            &store,
            &mut columns_cache,
            rule.as_ref().map(|rule| rule.board_id.as_str()),
            decision.outcome,
            &decision.detail,
            lang,
        )
        .await?;
        let verdict = RuleVerdict {
            rule: rule_label,
            outcome: outcome_word(decision.outcome, lang).to_string(),
            outcome_kind: decision.outcome.as_str().to_string(),
            detail,
        };
        if let Some(group) = decisions
            .iter_mut()
            .find(|g| g.event_id == decision.event_id)
        {
            group.verdicts.push(verdict);
            continue;
        }
        let task = store
            .task(&decision.task_id)
            .await?
            .map(|facts| format!("{} {}", facts.row.task_key, facts.row.title))
            .unwrap_or_else(|| t(lang, Key::TaskGoneLabel).to_string());
        let happened = event_happened(&store, &decision.event_id, lang).await?;
        decisions.push(DecisionGroup {
            event_id: decision.event_id,
            task,
            happened,
            at: izlek_core::detail::moment_label_in(decision.at, zone),
            verdicts: vec![verdict],
        });
    }

    let activity = store
        .recent_activity(LIMIT)
        .await?
        .into_iter()
        .map(|line| ActivityRow {
            at: izlek_core::detail::moment_label_in(line.at, zone),
            actor: line
                .actor_name
                .unwrap_or_else(|| t(lang, Key::TheSystem).to_string()),
            sentence: activity_sentence(&line.kind, &line.detail, lang),
            title: line.title,
        })
        .collect();

    Ok(Ok(LogsSnapshot {
        me: Me::from(&user),
        queue,
        decisions,
        activity,
    }))
}

/// The same read the page renders, as JSON — polled by `tests/http.rs`.
#[route(POST "/api/current_logs")]
async fn current_logs(cx: &Cx) -> Result<Json<std::result::Result<LogsSnapshot, Refusal>>> {
    Ok(Json(snapshot(cx).await?))
}

#[page("/logs")]
async fn logs_page(cx: &Cx) -> Result {
    let lang = Lang::En;
    match snapshot(cx).await {
        Ok(Ok(snapshot)) => logs_screen(cx, snapshot).await,
        // No `Me` here to read a language off of when the refusal itself is
        // "no session" — English, same as the other admin pages' own gate.
        Ok(Err(refusal)) => view! {
            cx =>
            <main class="scaffold-note">
                <p>(refusal.message())</p>
                <p><a href="/">(t(lang, Key::BackToBoard))</a></p>
            </main>
        },
        Err(_) => view! {
            cx =>
            <main class="scaffold-note">
                <p>(t(lang, Key::SomethingWentWrong))</p>
            </main>
        },
    }
}

async fn logs_screen(cx: &Cx, snapshot: LogsSnapshot) -> Result {
    let lang = Lang::from_code(&snapshot.me.language);
    let me = snapshot.me;
    let queue = snapshot.queue;
    let decisions = snapshot.decisions;
    let activity = snapshot.activity;
    let queue_empty = queue.is_empty();
    let decisions_empty = decisions.is_empty();
    let activity_empty = activity.is_empty();

    view! {
        cx =>
        <header class="topbar">
            <a class="wordmark" href="/">
                <span class="wordmark-text">"izlek"</span>
                <span class="wordmark-dot"></span>
            </a>
            (crate::layout::topbar_nav(cx, crate::layout::NavPage::Logs, lang).await?)
            <div class="spacer"></div>
            (crate::layout::user_menu(cx, &me, lang).await?)
        </header>

        <div class="settings-shell">
            <main class="settings-stage">
                <div class="settings-head">
                    <h1 class="settings-title">(t(lang, Key::Logs))</h1>
                    <span class="chip chip-admin">(t(lang, Key::AdminOnly))</span>
                </div>

                <section class="panel">
                    <div class="panel-head">
                        <h2 class="panel-title">(t(lang, Key::MailQueue))</h2>
                    </div>
                    <div class="panel-body">
                        <div class="rule-list rule-list-scroll">
                            for line in queue {
                                let is_failed_or_held = line.state_kind == "failed" || line.state_kind == "held";
                                let state_note = if is_failed_or_held {
                                    line.last_error.unwrap_or_else(|| line.state.clone())
                                } else {
                                    line.state.clone()
                                };
                                let chip_class = format!("rule-term rule-term-{}", line.state_kind);
                                let next_attempt = line.next_attempt.unwrap_or_else(|| t(lang, Key::NoRetry).to_string());
                                <div class="rule-row">
                                    <div class="rule-sentence">
                                        <span class="rule-term">(line.recipient)</span>
                                        <span>(line.subject)</span>
                                        <span class=(chip_class)>(state_note)</span>
                                        <span>(crate::i18n::attempt_label(lang, line.attempts))</span>
                                    </div>
                                    <span class="rule-stamp">(next_attempt)</span>
                                </div>
                            }
                            if queue_empty {
                                <p class="rules-quiet">(t(lang, Key::NothingOwed))</p>
                            }
                        </div>
                    </div>
                </section>

                <section class="panel">
                    <div class="panel-head">
                        <h2 class="panel-title">(t(lang, Key::MailDecisions))</h2>
                    </div>
                    <div class="panel-body">
                        <div class="rule-list rule-list-scroll">
                            for group in decisions {
                                let header = match group.happened {
                                    Some(happened) => format!("{} · {}", group.task, happened),
                                    None => group.task,
                                };
                                <div class="decision-block">
                                    <div class="decision-head">
                                        <span class="decision-title">(header)</span>
                                        <span class="rule-stamp">(group.at)</span>
                                    </div>
                                    <div class="decision-verdicts">
                                        for verdict in group.verdicts {
                                            let chip_class = format!("rule-term rule-term-{}", verdict.outcome_kind);
                                            <div class="decision-verdict">
                                                <span class="rule-term">(verdict.rule)</span>
                                                <span class=(chip_class)>(verdict.outcome)</span>
                                                <span>(verdict.detail)</span>
                                            </div>
                                        }
                                    </div>
                                </div>
                            }
                            if decisions_empty {
                                <p class="rules-quiet">(t(lang, Key::NoDecisionsYet))</p>
                            }
                        </div>
                    </div>
                </section>

                <section class="panel">
                    <div class="panel-head">
                        <h2 class="panel-title">(t(lang, Key::Activity))</h2>
                    </div>
                    <div class="panel-body">
                        <div class="rule-list rule-list-scroll">
                            for line in activity {
                                <div class="rule-row">
                                    <div class="rule-sentence">
                                        <span class="rule-term">(line.actor)</span>
                                        <span>(line.sentence)</span>
                                        <span>(line.title)</span>
                                    </div>
                                    <span class="rule-stamp">(line.at)</span>
                                </div>
                            }
                            if activity_empty {
                                <p class="rules-quiet">(t(lang, Key::NothingYet))</p>
                            }
                        </div>
                    </div>
                </section>
            </main>
        </div>
        (crate::layout::escape_script(cx).await?)
    }
}
