//! The rules engine: what a crossing owes, to whom, and what happened to it.
//!
//! The engine never decides on its own that a mail has already gone out. It
//! writes the ledger row first and lets the unique index answer — so an engine
//! that runs twice over one crossing, on a restart or from two workers, mails a
//! person once. Everything it says in a mail is read from committed facts,
//! including the moment the card moved: a send retried on Thursday still says
//! Tuesday.
//!
//! Nobody is ever mailed about their own action. A rule's audience is resolved
//! and the person who did the thing is taken off it, on every audience, always:
//! three people on one board do not need Izlek telling each of them what they
//! themselves just did. A rule whose audience is only the actor sends nothing
//! and owes nothing — that is not a failure and does not appear as one.
//!
//! None of that silence is undocumented any more: every rule an event touches
//! leaves a `mail_decision` row, win or not — disabled, not matched, an empty
//! audience, already owed, or freshly owed — so "why did nobody get mailed"
//! has a row to read instead of a log to search.
//!
//! Sending is deliberately outside the move's transaction. The crossing has to
//! commit whether or not a mail server is reachable, and a board that hangs for
//! thirty seconds because somebody's SMTP host is down is a board broken by its
//! mail feature.

use std::sync::Arc;

use async_trait::async_trait;
use time::{Duration, OffsetDateTime};

use crate::board::Transition;
use crate::store::{
    Audience, Event, Freeing, MailOutcome, MailRule, MailSend, SendKind, Store, Trigger,
};

/// Why the mail server would not take it, and whether that can change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailError {
    /// What the server said, kept as it was said.
    pub message: String,
    /// A timeout or a host that is down may work in a minute. A refused
    /// address will not, and retrying it forever is noise nobody reads.
    pub retryable: bool,
    /// Whether a mail server was actually asked. `false` means the mail never
    /// left the building — there is no sender configured yet — and that must
    /// not spend one of the five attempts, or a workspace whose admin sets the
    /// sender up tomorrow finds that today's mail was given up on overnight.
    pub attempted: bool,
}

impl MailError {
    pub fn retryable(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: true,
            attempted: true,
        }
    }

    pub fn permanent(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: false,
            attempted: true,
        }
    }

    /// Nothing was sent and nothing was asked: the workspace has no sender.
    /// The mail stays owed, at no cost to its attempts, and goes out when a
    /// sender exists — which is exactly what the empty-sender panel promises.
    pub fn unsent(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: true,
            attempted: false,
        }
    }
}

/// One mail, ready to go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outgoing {
    pub to: String,
    pub subject: String,
    pub body: String,
}

/// Whatever puts mail on the wire. The engine holds one of these and knows
/// nothing about SMTP; the tests hold one that remembers instead of sending.
#[async_trait]
pub trait Mailer: Send + Sync {
    async fn send(&self, mail: &Outgoing) -> Result<(), MailError>;
}

/// How many times a retryable failure is tried before it is given up on and
/// written down as abandoned.
pub const MAX_ATTEMPTS: u32 = 5;

/// How long a mail with no sender waits before it is looked at again. Short,
/// because the thing it waits on is an admin finishing a form and then watching
/// to see whether mail starts moving.
pub const HOLD: Duration = Duration::minutes(1);

/// The wait before the next attempt, doubling and capped: five minutes, ten,
/// twenty, forty, an hour.
pub fn backoff(attempts: u32) -> Duration {
    let minutes = 5_i64 * 2_i64.saturating_pow(attempts.saturating_sub(1).min(8));
    Duration::minutes(minutes.min(60))
}

/// What one engine run did, so a caller can log it and a test can assert on it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    /// Mails the server accepted.
    pub sent: u32,
    /// Mails this run did not own — somebody else already had them.
    pub already_owned: u32,
    /// Refusals that will be tried again.
    pub failed: u32,
    /// Refusals that will not.
    pub abandoned: u32,
    /// Mails held back because there is no sender to send them through. Owed,
    /// not failed: no attempt was spent and none will be until one exists.
    pub held: u32,
}

pub struct Engine {
    store: Arc<dyn Store>,
    mailer: Arc<dyn Mailer>,
    /// The origin links in mail point at, with no trailing slash.
    base_url: String,
}

impl Engine {
    pub fn new(
        store: Arc<dyn Store>,
        mailer: Arc<dyn Mailer>,
        base_url: impl Into<String>,
    ) -> Self {
        Self {
            store,
            mailer,
            base_url: base_url.into(),
        }
    }

    /// Sends one mail to the address given, right now, and says how long the
    /// mail server took to accept it.
    ///
    /// This is the only send that does not go through the ledger, and that is
    /// deliberate: a test mail is owed to nobody. If it fails there is nothing
    /// to retry and nothing to abandon — the person who pressed the button is
    /// watching, and the answer is for them.
    pub async fn send_test(&self, to: &str) -> std::result::Result<Duration, MailError> {
        let mail = Outgoing {
            to: to.to_string(),
            subject: "Izlek test mail".to_string(),
            body: "Somebody pressed the test button in Izlek's settings, and this \
                   is what came out. Nothing else was sent, and nobody else was \
                   written to.\n"
                .to_string(),
        };
        let started = std::time::Instant::now();
        self.mailer.send(&mail).await?;
        Ok(Duration::milliseconds(
            started.elapsed().as_millis().min(i64::MAX as u128) as i64,
        ))
    }

    /// Everything one crossing owes. Called after the move has committed, never
    /// inside it.
    pub async fn on_transition(&self, transition: &Transition) -> crate::store::Result<Report> {
        let mut report = Report::default();
        let Some(facts) = self.store.task(&transition.task_id).await? else {
            // A task deleted between the move and this run has no facts left
            // to read a board from `store.task`, but its rules still get a
            // `task_gone` row each — `board_of_task` reads through the soft
            // delete to find them.
            let Some(board_id) = self.store.board_of_task(&transition.task_id).await? else {
                return Ok(report);
            };
            let event = Event::Moved(transition.clone());
            for rule in self.store.mail_rules(&board_id).await? {
                self.store
                    .record_mail_decision(
                        &rule.id,
                        event.id(),
                        &transition.task_id,
                        MailOutcome::TaskGone,
                        "task was deleted before the mail ran",
                        event.at(),
                    )
                    .await?;
            }
            return Ok(report);
        };
        let rules = self.store.mail_rules(&facts.board_id).await?;
        let event = Event::Moved(transition.clone());
        let columns = self.store.columns_for_board(&facts.board_id).await?;
        let column_name = |id: &str| {
            columns
                .iter()
                .find(|column| column.id == id)
                .map(|column| column.name.clone())
                .unwrap_or_else(|| "a column".to_string())
        };

        for rule in &rules {
            if !rule.enabled {
                self.store
                    .record_mail_decision(
                        &rule.id,
                        event.id(),
                        &transition.task_id,
                        MailOutcome::Disabled,
                        "",
                        event.at(),
                    )
                    .await?;
                continue;
            }
            match &rule.trigger {
                Trigger::StatusBecomes(column) if *column == transition.to_column => {
                    self.owe(rule, &event, &transition.task_id, &mut report)
                        .await?;
                }
                Trigger::Unblocked => {
                    for freed in self.freed_by(transition, &facts.board_id).await? {
                        self.owe(rule, &event, &freed, &mut report).await?;
                    }
                }
                Trigger::StatusBecomes(watched) => {
                    let detail = format!(
                        "moved to {}, rule watches {}",
                        column_name(&transition.to_column),
                        column_name(watched)
                    );
                    self.store
                        .record_mail_decision(
                            &rule.id,
                            event.id(),
                            &transition.task_id,
                            MailOutcome::NotMatched,
                            &detail,
                            event.at(),
                        )
                        .await?;
                }
            }
        }
        Ok(report)
    }

    /// Everything a delete owes. A task stops being blocked in two ways and
    /// only one of them is a crossing: the blocker finishes, or the blocker is
    /// deleted. Both fire the unblocked rule, both claim through the same
    /// unique index, and the freed task is told once either way.
    ///
    /// Called after the delete has committed, with the tasks the store says it
    /// freed.
    pub async fn on_freeing(
        &self,
        freeing: &Freeing,
        freed: &[String],
    ) -> crate::store::Result<Report> {
        let mut report = Report::default();
        if freed.is_empty() {
            return Ok(report);
        }
        let rules = self.store.mail_rules(&freeing.board_id).await?;
        let event = Event::Freed(freeing.clone());
        let columns = self.store.columns_for_board(&freeing.board_id).await?;
        let column_name = |id: &str| {
            columns
                .iter()
                .find(|column| column.id == id)
                .map(|column| column.name.clone())
                .unwrap_or_else(|| "a column".to_string())
        };

        for rule in &rules {
            for task_id in freed {
                if !rule.enabled {
                    self.store
                        .record_mail_decision(
                            &rule.id,
                            event.id(),
                            task_id,
                            MailOutcome::Disabled,
                            "",
                            event.at(),
                        )
                        .await?;
                    continue;
                }
                match &rule.trigger {
                    Trigger::Unblocked => {
                        self.owe(rule, &event, task_id, &mut report).await?;
                    }
                    Trigger::StatusBecomes(watched) => {
                        let detail =
                            format!("freed a task, rule watches a move to {}", column_name(watched));
                        self.store
                            .record_mail_decision(
                                &rule.id,
                                event.id(),
                                task_id,
                                MailOutcome::NotMatched,
                                &detail,
                                event.at(),
                            )
                            .await?;
                    }
                }
            }
        }
        Ok(report)
    }

    /// Tasks this crossing let go of: ones this task was blocking, whose every
    /// blocker is now cleared or finished. A task still waiting on something
    /// else has not been unblocked, and telling its assignees to start would be
    /// a lie.
    async fn freed_by(
        &self,
        transition: &Transition,
        board_id: &str,
    ) -> crate::store::Result<Vec<String>> {
        // Only a crossing into a column that finishes work can free anything.
        let finished = self
            .store
            .columns_for_board(board_id)
            .await?
            .into_iter()
            .any(|column| column.id == transition.to_column && column.is_done);
        if !finished {
            return Ok(Vec::new());
        }

        let mut freed = Vec::new();
        for (blocked_by_other, edge) in self
            .store
            .dependencies_for_task(&transition.task_id)
            .await?
        {
            // `false` means the other task is the one waiting on this one.
            if blocked_by_other || edge.is_cleared() {
                continue;
            }
            let still_waiting = self
                .store
                .dependencies_for_task(&edge.task_id)
                .await?
                .into_iter()
                .any(|(is_blocked_by, other)| is_blocked_by && !other.is_cleared());
            if !still_waiting {
                freed.push(edge.task_id);
            }
        }
        Ok(freed)
    }

    /// Claims one mail per recipient and tries each one it owns.
    ///
    /// The person who caused the event is taken off the audience before
    /// anything is claimed: an empty audience leaves no ledger row at all, so
    /// a rule that only ever resolves to the actor is silent rather than being
    /// a row the admin has to read as a failure.
    async fn owe(
        &self,
        rule: &MailRule,
        event: &Event,
        task_id: &str,
        report: &mut Report,
    ) -> crate::store::Result<()> {
        let recipients = match rule.audience {
            Audience::Assignees => self.store.recipients_for_task(task_id).await?,
            Audience::Board => self.store.recipients_for_board(&rule.board_id).await?,
        };
        let resolved_nobody = recipients.is_empty();
        let now = OffsetDateTime::now_utc();
        let audience: Vec<_> = recipients
            .into_iter()
            .filter(|recipient| recipient.user_id != event.actor_id())
            .collect();
        if audience.is_empty() {
            let detail = if resolved_nobody {
                "audience is empty"
            } else {
                "audience was only the actor"
            };
            self.store
                .record_mail_decision(
                    &rule.id,
                    event.id(),
                    task_id,
                    MailOutcome::NoRecipients,
                    detail,
                    now,
                )
                .await?;
            return Ok(());
        }
        for recipient in audience {
            let claimed = self
                .store
                .claim_send(&rule.id, event.id(), task_id, &recipient.email, now)
                .await?;
            match claimed {
                Some(send) => {
                    self.store
                        .record_mail_decision(
                            &rule.id,
                            event.id(),
                            task_id,
                            MailOutcome::Owed,
                            "",
                            now,
                        )
                        .await?;
                    let mail = self.compose(&send, rule, event).await?;
                    self.attempt(&send, mail, report).await?;
                }
                None => {
                    self.store
                        .record_mail_decision(
                            &rule.id,
                            event.id(),
                            task_id,
                            MailOutcome::AlreadyOwed,
                            "",
                            now,
                        )
                        .await?;
                    report.already_owned += 1;
                }
            }
        }
        Ok(())
    }

    /// Sends what is owed and is due: whatever failed earlier and has waited
    /// long enough, plus anything a crash left claimed but unsent.
    pub async fn deliver_owed(
        &self,
        now: OffsetDateTime,
        limit: u32,
    ) -> crate::store::Result<Report> {
        let mut report = Report::default();
        for send in self.store.sends_owed(now, limit).await? {
            // An invite owes no rule and no event: it carries its own subject
            // and body, composed once at mint time.
            if send.kind == SendKind::Invite {
                let mail = Outgoing {
                    to: send.recipient.clone(),
                    subject: send.subject.clone().unwrap_or_default(),
                    body: send.body.clone().unwrap_or_default(),
                };
                self.attempt(&send, Some(mail), &mut report).await?;
                continue;
            }
            let Some(rule_id) = send.rule_id.as_deref() else {
                continue;
            };
            let Some(event_id) = send.event_id.as_deref() else {
                continue;
            };
            let Some(rule) = self.store.mail_rule(rule_id).await? else {
                continue;
            };
            let Some(event) = self.store.event(event_id).await? else {
                continue;
            };
            let mail = self.compose(&send, &rule, &event).await?;
            self.attempt(&send, mail, &mut report).await?;
        }
        Ok(report)
    }

    /// One attempt, and the ledger line it leaves behind.
    async fn attempt(
        &self,
        send: &MailSend,
        mail: Option<Outgoing>,
        report: &mut Report,
    ) -> crate::store::Result<()> {
        let Some(mail) = mail else {
            return Ok(());
        };
        let now = OffsetDateTime::now_utc();
        match self.mailer.send(&mail).await {
            Ok(()) => {
                self.store.record_send_accepted(&send.id, now).await?;
                report.sent += 1;
            }
            Err(problem) if !problem.attempted => {
                // No sender. The mail keeps its place in the ledger and its
                // attempts, and is looked at again on the next sweep — by then
                // an admin may have filled the panel in.
                self.store
                    .defer_send(&send.id, &problem.message, now + HOLD, now)
                    .await?;
                report.held += 1;
            }
            Err(problem) => {
                let attempts = send.attempts + 1;
                // The last attempt is written down as given up on rather than
                // left looking like it is still coming.
                let retry_at =
                    (problem.retryable && attempts < MAX_ATTEMPTS).then(|| now + backoff(attempts));
                self.store
                    .record_send_refused(&send.id, &problem.message, retry_at, now)
                    .await?;
                if retry_at.is_some() {
                    report.failed += 1;
                } else {
                    report.abandoned += 1;
                }
            }
        }
        Ok(())
    }

    /// The mail itself, built from the facts as they were committed.
    async fn compose(
        &self,
        send: &MailSend,
        rule: &MailRule,
        event: &Event,
    ) -> crate::store::Result<Option<Outgoing>> {
        let Some(task_id) = send.task_id.as_deref() else {
            return Ok(None);
        };
        let Some(facts) = self.store.task(task_id).await? else {
            return Ok(None);
        };
        let actor = self
            .store
            .user(event.actor_id())
            .await?
            .map(|user| user.display_name)
            .unwrap_or_else(|| "Somebody".to_string());
        let columns = self.store.columns_for_board(&facts.board_id).await?;
        let named = |id: &str| {
            columns
                .iter()
                .find(|column| column.id == id)
                .map(|column| column.name.clone())
                .unwrap_or_else(|| "a column".to_string())
        };
        let happened = match (event, &rule.trigger) {
            (Event::Moved(transition), Trigger::StatusBecomes(_)) => format!(
                "{} moved it from {} to {}.",
                actor,
                named(&transition.from_column),
                named(&transition.to_column)
            ),
            (Event::Moved(_), Trigger::Unblocked) => {
                format!("{} finished the last task it was waiting on.", actor)
            }
            (Event::Freed(freeing), _) => format!(
                "{} deleted {} — {}, the last task it was waiting on.",
                actor, freeing.cause_key, freeing.cause_title
            ),
        };
        let body = format!(
            "{key} — {title}\n\n{happened}\n\n{when}\n\n{base}/?task={id}\n",
            key = facts.row.task_key,
            title = facts.row.title,
            when = day_and_time(event.at()),
            base = self.base_url,
            id = facts.row.id,
        );
        Ok(Some(Outgoing {
            to: send.recipient.clone(),
            subject: rule.subject.clone(),
            body,
        }))
    }
}

/// `Aug 26 at 11:04 UTC` — the crossing's own clock, not the sender's.
fn day_and_time(at: OffsetDateTime) -> String {
    format!(
        "{} {} at {:02}:{:02} UTC",
        month_name(at.month()),
        at.day(),
        at.hour(),
        at.minute()
    )
}

fn month_name(month: time::Month) -> &'static str {
    use time::Month::*;
    match month {
        January => "Jan",
        February => "Feb",
        March => "Mar",
        April => "Apr",
        May => "May",
        June => "Jun",
        July => "Jul",
        August => "Aug",
        September => "Sep",
        October => "Oct",
        November => "Nov",
        December => "Dec",
    }
}

#[cfg(test)]
mod backoff_tests {
    use super::{MAX_ATTEMPTS, backoff};
    use time::Duration;

    #[test]
    fn the_wait_grows_and_then_stops_growing() {
        assert_eq!(backoff(1), Duration::minutes(5));
        assert_eq!(backoff(2), Duration::minutes(10));
        assert_eq!(backoff(3), Duration::minutes(20));
        assert_eq!(backoff(4), Duration::minutes(40));
        assert_eq!(backoff(5), Duration::minutes(60));
        // Nothing waits longer than an hour, whatever it is handed.
        assert_eq!(backoff(50), Duration::minutes(60));
        assert_eq!(backoff(u32::MAX), Duration::minutes(60));
    }

    #[test]
    fn a_send_is_tried_a_bounded_number_of_times() {
        assert_eq!(MAX_ATTEMPTS, 5);
    }
}
