//! The admin's Logs screen: what mail is owed, what the rules decided, and
//! what happened across the workspace. Read-only — nothing here is written
//! back, so unlike Mail rules there is no action to guard beyond the read
//! itself.

use leptos::prelude::*;
use serde::{Deserialize, Serialize};

use crate::auth::{Me, Refusal};

/// One send still owed or refused, as the queue panel reads it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueLine {
    pub recipient: String,
    pub subject: String,
    /// "pending" or "failed".
    pub state: String,
    pub attempts: u32,
    pub last_error: Option<String>,
    pub next_attempt: Option<String>,
}

/// One rule decision, as the decisions panel reads it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionLine {
    pub at: String,
    pub task: String,
    pub outcome: String,
    pub detail: String,
}

/// One line of the workspace activity feed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityRow {
    pub at: String,
    pub actor: String,
    pub sentence: String,
    pub title: String,
}

/// The screen in one answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogsSnapshot {
    pub me: Me,
    pub queue: Vec<QueueLine>,
    pub decisions: Vec<DecisionLine>,
    pub activity: Vec<ActivityRow>,
}

/// How many mail is owed, what the rules decided, and what happened, all
/// capped so the screen stays a read a person can finish.
#[cfg(feature = "ssr")]
const LIMIT: u32 = 50;

/// The word the decisions panel prints for an outcome. Terse: the panel is
/// read, not explained.
#[cfg(feature = "ssr")]
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
#[cfg(feature = "ssr")]
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
        ActivityKind::Other(_) => detail.to_string(),
    }
}

/// The queue, the decisions, and the activity, admin only.
#[server]
pub async fn current_logs() -> Result<Result<LogsSnapshot, Refusal>, ServerFnError> {
    use crate::server::{accounts, require_admin};
    use izlek_core::store::SendState;

    let user = match require_admin().await {
        Ok(user) => user,
        Err(refusal) => return Ok(Err(refusal)),
    };
    let store = accounts().store().clone();
    let fail = |e: izlek_core::store::StoreError| ServerFnError::new(e.to_string());

    let sends = store.mail_queue(LIMIT).await.map_err(fail)?;
    let mut queue = Vec::with_capacity(sends.len());
    for send in sends {
        let subject = store
            .mail_rule(&send.rule_id)
            .await
            .map_err(fail)?
            .map(|rule| rule.subject)
            .unwrap_or_else(|| "a rule that is gone".to_string());
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
            next_attempt: send.next_attempt_at.map(izlek_core::detail::moment_label),
        });
    }

    let raw_decisions = store.recent_mail_decisions(LIMIT).await.map_err(fail)?;
    let mut decisions = Vec::with_capacity(raw_decisions.len());
    for decision in raw_decisions {
        let task = store
            .task(&decision.task_id)
            .await
            .map_err(fail)?
            .map(|facts| format!("{} {}", facts.row.task_key, facts.row.title))
            .unwrap_or_else(|| "a task that is gone".to_string());
        decisions.push(DecisionLine {
            at: izlek_core::detail::moment_label(decision.at),
            task,
            outcome: outcome_word(decision.outcome).to_string(),
            detail: decision.detail,
        });
    }

    let activity = store
        .recent_activity(LIMIT)
        .await
        .map_err(fail)?
        .into_iter()
        .map(|line| ActivityRow {
            at: izlek_core::detail::moment_label(line.at),
            actor: line.actor_name.unwrap_or_else(|| "The system".to_string()),
            sentence: activity_sentence(&line.kind, &line.detail),
            title: line.title,
        })
        .collect();

    Ok(Ok(LogsSnapshot {
        me: Me {
            id: user.id,
            display_name: user.display_name,
            email: user.email,
            role: user.role,
        },
        queue,
        decisions,
        activity,
    }))
}

#[component]
pub fn LogsPage() -> impl IntoView {
    let logs = Resource::new(|| (), |_| async move { current_logs().await });

    view! {
        <Transition fallback=|| view! { <main class="settings-stage"></main> }>
            {move || Suspend::new(async move {
                match logs.await {
                    Ok(Ok(snapshot)) => view! { <LogsScreen snapshot=snapshot/> }.into_any(),
                    Ok(Err(refusal)) => {
                        view! {
                            <main class="scaffold-note">
                                <p>{refusal.message()}</p>
                                <p>
                                    <a href="/">"Back to the board"</a>
                                </p>
                            </main>
                        }
                            .into_any()
                    }
                    Err(_) => {
                        view! {
                            <main class="scaffold-note">
                                <p>"Something went wrong."</p>
                            </main>
                        }
                            .into_any()
                    }
                }
            })}
        </Transition>
    }
}

#[component]
fn LogsScreen(snapshot: LogsSnapshot) -> impl IntoView {
    let me = snapshot.me.clone();

    let queue_empty = snapshot.queue.is_empty();
    let queue_rows = snapshot
        .queue
        .into_iter()
        .map(|line| {
            let state_note = if line.state == "failed" || line.state == "held" {
                line.last_error.unwrap_or_else(|| line.state.clone())
            } else {
                line.state.clone()
            };
            view! {
                <div class="rule-row">
                    <div class="rule-sentence">
                        <span class="rule-term">{line.recipient}</span>
                        <span>{line.subject}</span>
                        <span class="rule-term">{state_note}</span>
                        <span>{format!("attempt {}", line.attempts)}</span>
                    </div>
                    <span class="rule-stamp">
                        {line.next_attempt.unwrap_or_else(|| "no retry".to_string())}
                    </span>
                </div>
            }
        })
        .collect_view();

    let decisions_empty = snapshot.decisions.is_empty();
    let decision_rows = snapshot
        .decisions
        .into_iter()
        .map(|line| {
            view! {
                <div class="rule-row">
                    <div class="rule-sentence">
                        <span>{line.task}</span>
                        <span class="rule-term">{line.outcome}</span>
                        <span>{line.detail}</span>
                    </div>
                    <span class="rule-stamp">{line.at}</span>
                </div>
            }
        })
        .collect_view();

    let activity_empty = snapshot.activity.is_empty();
    let activity_rows = snapshot
        .activity
        .into_iter()
        .map(|line| {
            view! {
                <div class="rule-row">
                    <div class="rule-sentence">
                        <span class="rule-term">{line.actor}</span>
                        <span>{line.sentence}</span>
                        <span>{line.title}</span>
                    </div>
                    <span class="rule-stamp">{line.at}</span>
                </div>
            }
        })
        .collect_view();

    view! {
        <header class="topbar">
            <div class="wordmark">
                <span class="wordmark-text">"izlek"</span>
                <span class="wordmark-dot"></span>
            </div>
            <div class="topbar-divider"></div>
            <span class="board-name">"Logs"</span>
            <div class="spacer"></div>
            <span class="topbar-who" title=me.email.clone()>
                {me.display_name.clone()}
            </span>
        </header>

        <div class="settings-shell">
            <nav class="sidenav">
                <a class="sidenav-item" href="/">
                    "Board"
                </a>
                <a class="sidenav-item" href="/rules">
                    "Mail rules"
                </a>
                <a class="sidenav-item sidenav-item-on" href="/logs">
                    "Logs"
                </a>
                <a class="sidenav-item" href="/settings">
                    "Settings"
                </a>
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
                            {queue_rows}
                            {queue_empty.then(|| view! { <p class="rules-quiet">"Nothing owed."</p> })}
                        </div>
                    </div>
                </section>

                <section class="panel">
                    <div class="panel-head">
                        <h2 class="panel-title">"Mail decisions"</h2>
                    </div>
                    <div class="panel-body">
                        <div class="rule-list">
                            {decision_rows}
                            {decisions_empty
                                .then(|| view! { <p class="rules-quiet">"No decisions yet."</p> })}
                        </div>
                    </div>
                </section>

                <section class="panel">
                    <div class="panel-head">
                        <h2 class="panel-title">"Activity"</h2>
                    </div>
                    <div class="panel-body">
                        <div class="rule-list">
                            {activity_rows}
                            {activity_empty.then(|| view! { <p class="rules-quiet">"Nothing yet."</p> })}
                        </div>
                    </div>
                </section>
            </main>
        </div>
    }
}
