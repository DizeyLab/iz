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
use crate::server::{Refusal, accounts, require_admin};

/// One send still owed or refused, as the queue panel reads it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct QueueLine {
    recipient: String,
    subject: String,
    /// "pending" or "failed".
    state: String,
    attempts: u32,
    last_error: Option<String>,
    next_attempt: Option<String>,
}

/// One rule's verdict inside a decision block.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct RuleVerdict {
    rule: String,
    outcome: String,
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
fn outcome_word(outcome: izlek_core::store::MailOutcome) -> &'static str {
    use izlek_core::store::MailOutcome;
    match outcome {
        MailOutcome::Owed => "queued",
        MailOutcome::AlreadyOwed => "already queued",
        MailOutcome::NoRecipients => "nobody to mail",
        MailOutcome::NotMatched => "did not match",
        MailOutcome::Disabled => "rule off",
        MailOutcome::TaskGone => "task gone",
    }
}

/// The sentence after a task's name, mirroring `ActivityEntry::sentence` —
/// the per-task strip on the detail screen — since the workspace feed carries
/// the same kind and detail but no `Person` to build an `ActivityEntry` from.
fn activity_sentence(kind: &izlek_core::detail::ActivityKind, detail: &str) -> String {
    use izlek_core::detail::ActivityKind;
    let detail = detail.trim();
    match kind {
        ActivityKind::Created => "created this task".to_string(),
        ActivityKind::Retitled => "renamed this task".to_string(),
        ActivityKind::Described => "edited the description".to_string(),
        ActivityKind::DeadlineSet => format!("set deadline {detail}"),
        ActivityKind::DeadlineCleared => "removed the deadline".to_string(),
        ActivityKind::Assigned => format!("assigned {detail}"),
        ActivityKind::Unassigned => format!("unassigned {detail}"),
        ActivityKind::Linked => format!("linked {detail}"),
        ActivityKind::Unlinked => format!("unlinked {detail}"),
        ActivityKind::Moved => format!("moved {detail}"),
        ActivityKind::Unblocked => format!("unblocked this task — {detail}"),
        ActivityKind::Deleted => "deleted this task".to_string(),
        ActivityKind::Commented => "commented".to_string(),
        ActivityKind::Other(_) => detail.to_string(),
    }
}

/// What an event did, in the decisions panel's own words — "moved to
/// Review" — or nothing when the event or its destination column is gone:
/// the panel names what it can still read, never a guess.
async fn event_happened(
    store: &std::sync::Arc<dyn izlek_core::store::Store>,
    event_id: &str,
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
                .map(|column| format!("moved to {}", column.name)))
        }
        Event::Freed(_) => Ok(Some("unblocked".to_string())),
        // Not a wired path yet — S4's work — so this only needs to be
        // exhaustive, not right.
        Event::Happened(activity) => Ok(Some(activity.kind.as_str().to_string())),
    }
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
                    .unwrap_or_else(|| "a rule that is gone".to_string()),
                None => "a rule that is gone".to_string(),
            },
        };
        queue.push(QueueLine {
            recipient: send.recipient,
            subject,
            state: match send.state {
                SendState::Pending => "pending".to_string(),
                // A send the engine never attempted is only held — usually
                // for want of a sender — not failed at anything.
                SendState::Failed if send.attempts == 0 => "held".to_string(),
                SendState::Failed => "failed".to_string(),
                SendState::Sent => "sent".to_string(),
                SendState::Abandoned => "abandoned".to_string(),
            },
            attempts: send.attempts,
            last_error: send.last_error,
            next_attempt: send
                .next_attempt_at
                .map(|at| izlek_core::detail::moment_label_in(at, zone)),
        });
    }

    let raw_decisions = store.recent_mail_decisions(LIMIT).await?;
    let mut decisions: Vec<DecisionGroup> = Vec::new();
    for decision in raw_decisions {
        let rule = store
            .mail_rule(&decision.rule_id)
            .await?
            .map(|rule| rule.subject)
            .unwrap_or_else(|| "a rule that is gone".to_string());
        let verdict = RuleVerdict {
            rule,
            outcome: outcome_word(decision.outcome).to_string(),
            detail: decision.detail,
        };
        if let Some(group) = decisions.iter_mut().find(|g| g.event_id == decision.event_id) {
            group.verdicts.push(verdict);
            continue;
        }
        let task = store
            .task(&decision.task_id)
            .await?
            .map(|facts| format!("{} {}", facts.row.task_key, facts.row.title))
            .unwrap_or_else(|| "a task that is gone".to_string());
        let happened = event_happened(&store, &decision.event_id).await?;
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
            actor: line.actor_name.unwrap_or_else(|| "The system".to_string()),
            sentence: activity_sentence(&line.kind, &line.detail),
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
    match snapshot(cx).await {
        Ok(Ok(snapshot)) => logs_screen(cx, snapshot).await,
        Ok(Err(refusal)) => view! {
            cx =>
            <main class="scaffold-note">
                <p>(refusal.message())</p>
                <p><a href="/">"Back to the board"</a></p>
            </main>
        },
        Err(_) => view! {
            cx =>
            <main class="scaffold-note">
                <p>"Something went wrong."</p>
            </main>
        },
    }
}

async fn logs_screen(cx: &Cx, snapshot: LogsSnapshot) -> Result {
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
            <div class="topbar-divider"></div>
            <span class="board-name">"Logs"</span>
            <div class="spacer"></div>
            <span class="topbar-who" title=(me.email)>(me.display_name)</span>
        </header>

        <div class="settings-shell">
            <nav class="sidenav">
                <a class="sidenav-item" href="/">"Board"</a>
                <a class="sidenav-item" href="/rules">"Mail rules"</a>
                <a class="sidenav-item sidenav-item-on" href="/logs">"Logs"</a>
                <a class="sidenav-item" href="/settings">"Settings"</a>
            </nav>

            <main class="settings-stage">
                <div class="settings-head">
                    <h1 class="settings-title">"Logs"</h1>
                    <span class="chip chip-admin">"Admin only"</span>
                </div>

                <section class="panel">
                    <div class="panel-head">
                        <h2 class="panel-title">"Mail queue"</h2>
                    </div>
                    <div class="panel-body">
                        <div class="rule-list">
                            for line in queue {
                                let state_note = if line.state == "failed" || line.state == "held" {
                                    line.last_error.unwrap_or_else(|| line.state.clone())
                                } else {
                                    line.state.clone()
                                };
                                let next_attempt = line.next_attempt.unwrap_or_else(|| "no retry".to_string());
                                <div class="rule-row">
                                    <div class="rule-sentence">
                                        <span class="rule-term">(line.recipient)</span>
                                        <span>(line.subject)</span>
                                        <span class="rule-term">(state_note)</span>
                                        <span>(format!("attempt {}", line.attempts))</span>
                                    </div>
                                    <span class="rule-stamp">(next_attempt)</span>
                                </div>
                            }
                            if queue_empty {
                                <p class="rules-quiet">"Nothing owed."</p>
                            }
                        </div>
                    </div>
                </section>

                <section class="panel">
                    <div class="panel-head">
                        <h2 class="panel-title">"Mail decisions"</h2>
                    </div>
                    <div class="panel-body">
                        <div class="rule-list">
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
                                            let chip_class = if verdict.outcome == "queued" {
                                                "rule-term rule-term-queued"
                                            } else {
                                                "rule-term"
                                            };
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
                                <p class="rules-quiet">"No decisions yet."</p>
                            }
                        </div>
                    </div>
                </section>

                <section class="panel">
                    <div class="panel-head">
                        <h2 class="panel-title">"Activity"</h2>
                    </div>
                    <div class="panel-body">
                        <div class="rule-list">
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
                                <p class="rules-quiet">"Nothing yet."</p>
                            }
                        </div>
                    </div>
                </section>
            </main>
        </div>
    }
}
