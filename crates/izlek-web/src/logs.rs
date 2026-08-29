//! The admin's Logs screen: what mail is owed, what the rules decided, and
//! what happened across the workspace. Read-only — nothing here is written
//! back, so unlike Mail rules there is no action to guard beyond the read
//! itself.
//!
//! Ported from the old UI's `logs.rs`. That version read through a
//! server fn behind a `Resource`; here the page is rendered server-side on
//! every request, so `snapshot` — the shared read — backs both the page and
//! the JSON route `tests/http.rs` polls.

use izlek_core::store::{ActivityFilter, Dir, FeedCursor, FeedPage};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use topcoat::Result;
use topcoat::context::Cx;
use topcoat::router::content::Json;
use topcoat::router::{page, route};
use topcoat::view::view;

use crate::detail::{Me, datepicker_grid};
use crate::i18n::{Key, Lang, t};
use crate::server::{Refusal, accounts, require_admin};
use crate::settings::{decode_q, encode_q};

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
    title: Option<String>,
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

/// Every `ActivityKind::as_str()` value but `Other` — the Type filter's
/// options.
const ACTIVITY_KINDS: &[&str] = &[
    "created",
    "retitled",
    "described",
    "deadline_set",
    "deadline_cleared",
    "assigned",
    "unassigned",
    "linked",
    "unlinked",
    "moved",
    "unblocked",
    "deleted",
    "commented",
    "workspace_claimed",
    "invited",
    "link_resent",
    "joined",
    "signed_in",
    "signed_out",
    "sign_in_failed",
    "password_changed",
    "role_changed",
    "profile_saved",
    "sender_saved",
    "limits_saved",
    "test_mail_sent",
    "rule_created",
    "rule_edited",
    "rule_toggled",
    "rule_deleted",
    "file_added",
    "file_removed",
];

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
        ActivityKind::FileAdded => crate::i18n::file_added_label(lang, detail),
        ActivityKind::FileRemoved => crate::i18n::file_removed_label(lang, detail),
        ActivityKind::WorkspaceClaimed => t(lang, Key::ActWorkspaceClaimed).to_string(),
        ActivityKind::Invited => crate::i18n::invited_label(lang, detail),
        ActivityKind::LinkResent => crate::i18n::link_resent_label(lang, detail),
        ActivityKind::Joined => t(lang, Key::ActJoined).to_string(),
        ActivityKind::SignedIn => t(lang, Key::ActSignedIn).to_string(),
        ActivityKind::SignInFailed => crate::i18n::sign_in_failed_label(lang, detail),
        ActivityKind::SignedOut => t(lang, Key::ActSignedOut).to_string(),
        ActivityKind::PasswordChanged => t(lang, Key::ActPasswordChanged).to_string(),
        ActivityKind::ProfileSaved => t(lang, Key::ActProfileSaved).to_string(),
        ActivityKind::SenderSaved => t(lang, Key::ActSenderSaved).to_string(),
        ActivityKind::LimitsSaved => t(lang, Key::ActLimitsSaved).to_string(),
        ActivityKind::TestMailSent => t(lang, Key::ActTestMailSent).to_string(),
        ActivityKind::RoleChanged => crate::i18n::role_changed_label(lang, detail),
        ActivityKind::RuleCreated => crate::i18n::rule_created_label(lang, detail),
        ActivityKind::RuleEdited => crate::i18n::rule_edited_label(lang, detail),
        ActivityKind::RuleToggled => crate::i18n::rule_toggled_label(lang, detail),
        ActivityKind::RuleDeleted => crate::i18n::rule_deleted_label(lang, detail),
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

/// Query width and cursor for one section's list: the extra row beyond
/// `limit` only signals that an older page exists — it is trimmed before
/// rendering. Only the section actually showing is paged, at its own
/// `limit`; the rest always read the newest `LIMIT`.
fn feed_window(active: Section, target: Section, page: &FeedPage, limit: u32) -> (u32, FeedPage) {
    if active == target {
        (limit + 1, page.clone())
    } else {
        (LIMIT, FeedPage::Newest)
    }
}

/// The active section's own page size: from `izlek_rows_<section>`, the
/// cookie the page's own fit script sets once it has measured the browser's
/// real viewport, clamped so a stale or tampered cookie can't ask for an
/// absurd window. `LIMIT` — the same default the unpaged JSON route reads —
/// when the cookie is absent or unparsable.
fn resolve_limit(cx: &Cx, section: Section) -> u32 {
    crate::server::presented_cookie(cx, &format!("izlek_rows_{}", section_slug(section)))
        .and_then(|raw| raw.parse::<u32>().ok())
        .map(|rows| rows.clamp(10, 200))
        .unwrap_or(LIMIT)
}

/// Where the reader can turn from the active section's current page: the
/// query fragment (`before=…`/`after=…`, already encoded) for each
/// direction the rail may still show a link for.
struct PageLinks {
    show_newer: bool,
    show_older: bool,
    newer_q: Option<String>,
    older_q: Option<String>,
    /// (X, Y, N) for the active section's "X–Y / N" note: rendered rows are
    /// X through Y of N matching rows. Absent when the list is empty.
    position: Option<(u64, u64, u64)>,
}

fn cursor_q(param: &str, cursor: &FeedCursor) -> String {
    let raw = format!(
        "{}~{}",
        cursor.at.format(&Rfc3339).unwrap_or_default(),
        cursor.id
    );
    format!("{param}={}", encode_q(&raw))
}

fn parse_cursor(raw: &str) -> Option<FeedCursor> {
    let decoded = decode_q(raw);
    let (at, id) = decoded.rsplit_once('~')?;
    let at = OffsetDateTime::parse(at, &Rfc3339).ok()?;
    Some(FeedCursor { at, id: id.to_string() })
}

/// `before=`/`after=` from the query: mutually exclusive, and either one
/// absent or unparsable falls back to the newest page.
fn parse_page(query: &str) -> FeedPage {
    match (query_value(query, "before"), query_value(query, "after")) {
        (Some(raw), None) => parse_cursor(raw).map(FeedPage::Before).unwrap_or(FeedPage::Newest),
        (None, Some(raw)) => parse_cursor(raw).map(FeedPage::After).unwrap_or(FeedPage::Newest),
        _ => FeedPage::Newest,
    }
}

/// `dir=oldest` reverses the activity tab; anything else, including
/// absence, reads newest first.
fn parse_dir(query: &str) -> Dir {
    match query_value(query, "dir") {
        Some("oldest") => Dir::Oldest,
        _ => Dir::Newest,
    }
}

/// `on=YYYY-MM-DD`, resolved to a half-open UTC range in the admin's own
/// timezone. Unparsable or absent is no filter, never a 500.
fn parse_day(raw: &str, zone: time::UtcOffset) -> Option<(OffsetDateTime, OffsetDateTime)> {
    use time::macros::format_description;
    let day = time::Date::parse(raw, format_description!("[year]-[month]-[day]")).ok()?;
    let start = day
        .with_hms(0, 0, 0)
        .ok()?
        .assume_offset(zone)
        .to_offset(time::UtcOffset::UTC);
    Some((start, start + time::Duration::hours(24)))
}

/// The activity tab's filter, straight off the query: an absent or empty
/// value narrows nothing.
fn parse_activity_filter(query: &str, zone: time::UtcOffset) -> ActivityFilter {
    ActivityFilter {
        actor: query_value(query, "actor").filter(|v| !v.is_empty()).map(str::to_string),
        kind: query_value(query, "kind").filter(|v| !v.is_empty()).map(str::to_string),
        task_key: query_value(query, "task")
            .filter(|v| !v.is_empty())
            .map(|v| v.to_uppercase()),
        day: query_value(query, "on").and_then(|raw| parse_day(raw, zone)),
    }
}

/// Every active activity-filter param, as a query fragment (leading `&`,
/// empty when nothing is set) — round-tripped onto cursor hrefs and the
/// filter form's own action so a page turn never drops a filter.
fn activity_query_suffix(query: &str) -> String {
    let mut out = String::new();
    for key in ["actor", "kind", "task", "on", "dir"] {
        if let Some(value) = query_value(query, key).filter(|v| !v.is_empty()) {
            out.push('&');
            out.push_str(key);
            out.push('=');
            out.push_str(value);
        }
    }
    out
}

/// The queue, the decisions, and the activity, admin only. Shared by the
/// `/logs` page and the `/api/current_logs` route so both answer the same
/// read the same way. `active`/`page` widen and cursor only the section
/// being paged; the JSON caller always passes `FeedPage::Newest`, matching
/// the old unpaged read.
async fn snapshot(
    cx: &Cx,
    active: Section,
    page: FeedPage,
    dir: Dir,
    filter: &ActivityFilter,
    limit: u32,
) -> Result<std::result::Result<(LogsSnapshot, PageLinks), Refusal>> {
    use izlek_core::store::SendState;

    let user = match require_admin(cx).await {
        Ok(user) => user,
        Err(refusal) => return Ok(Err(refusal)),
    };
    let lang = Lang::from_code(&user.language);
    let zone = izlek_core::detail::parse_zone(&user.timezone);
    let store = accounts(cx).store().clone();

    let mut has_more = false;
    // Only the active section can move off `Newest`; a cursor that ran off
    // the top on a retried fetch below updates this so the links reflect
    // where the page actually landed rather than where it was asked to go.
    let mut effective_page = page.clone();
    let mut newer_cursor: Option<FeedCursor> = None;
    let mut older_cursor: Option<FeedCursor> = None;

    let (queue_limit, queue_page) = feed_window(active, Section::Queue, &page, limit);
    let mut sends = store.mail_queue(queue_limit, queue_page).await?;
    if active == Section::Queue && matches!(page, FeedPage::After(_)) && sends.is_empty() {
        effective_page = FeedPage::Newest;
        sends = store.mail_queue(limit + 1, FeedPage::Newest).await?;
    }
    if active == Section::Queue && sends.len() as u32 > limit {
        has_more = true;
        sends.truncate(limit as usize);
    }
    if active == Section::Queue {
        newer_cursor = sends
            .first()
            .and_then(|s| s.next_attempt_at.map(|at| FeedCursor { at, id: s.id.clone() }));
        older_cursor = sends
            .last()
            .and_then(|s| s.next_attempt_at.map(|at| FeedCursor { at, id: s.id.clone() }));
    }
    let queue_shown = sends.len() as u64;
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

    let (decisions_limit, decisions_page) = feed_window(active, Section::Decisions, &page, limit);
    let mut raw_decisions = store.recent_mail_decisions(decisions_limit, decisions_page).await?;
    if active == Section::Decisions && matches!(page, FeedPage::After(_)) && raw_decisions.is_empty()
    {
        effective_page = FeedPage::Newest;
        raw_decisions = store.recent_mail_decisions(limit + 1, FeedPage::Newest).await?;
    }
    if active == Section::Decisions && raw_decisions.len() as u32 > limit {
        has_more = true;
        raw_decisions.truncate(limit as usize);
    }
    // Cursors come from the raw rows, before grouping by event — a group
    // split across a page boundary is accepted, unchanged from the offset
    // scheme this replaces.
    if active == Section::Decisions {
        newer_cursor = raw_decisions
            .first()
            .map(|d| FeedCursor { at: d.at, id: d.id.clone() });
        older_cursor = raw_decisions
            .last()
            .map(|d| FeedCursor { at: d.at, id: d.id.clone() });
    }
    let decisions_shown = raw_decisions.len() as u64;
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

    let (activity_limit, activity_page) = feed_window(active, Section::Activity, &page, limit);
    let activity_filter = if active == Section::Activity {
        filter.clone()
    } else {
        ActivityFilter::default()
    };
    let mut raw_activity =
        store.recent_activity(activity_limit, activity_page, dir, &activity_filter).await?;
    if active == Section::Activity && matches!(page, FeedPage::After(_)) && raw_activity.is_empty()
    {
        effective_page = FeedPage::Newest;
        raw_activity =
            store.recent_activity(limit + 1, FeedPage::Newest, dir, &activity_filter).await?;
    }
    if active == Section::Activity && raw_activity.len() as u32 > limit {
        has_more = true;
        raw_activity.truncate(limit as usize);
    }
    if active == Section::Activity {
        newer_cursor = raw_activity
            .first()
            .map(|line| FeedCursor { at: line.at, id: line.id.clone() });
        older_cursor = raw_activity
            .last()
            .map(|line| FeedCursor { at: line.at, id: line.id.clone() });
    }
    let activity_shown = raw_activity.len() as u64;
    let activity = raw_activity
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

    let show_older = matches!(effective_page, FeedPage::After(_)) || has_more;
    let show_newer = matches!(effective_page, FeedPage::Before(_))
        || (matches!(effective_page, FeedPage::After(_)) && has_more);

    let position = match active {
        Section::Queue if queue_shown > 0 => {
            let total = store.count_mail_queue().await?;
            let preceding = store.count_mail_queue_preceding(newer_cursor.as_ref()).await?;
            Some((preceding + 1, preceding + queue_shown, total))
        }
        Section::Decisions if decisions_shown > 0 => {
            let total = store.count_mail_decisions().await?;
            let preceding = store.count_mail_decisions_preceding(newer_cursor.as_ref()).await?;
            Some((preceding + 1, preceding + decisions_shown, total))
        }
        Section::Activity if activity_shown > 0 => {
            let total = store.count_activity(&activity_filter).await?;
            let preceding = store
                .count_activity_preceding(&activity_filter, dir, newer_cursor.as_ref())
                .await?;
            Some((preceding + 1, preceding + activity_shown, total))
        }
        _ => None,
    };

    let links = PageLinks {
        show_newer,
        show_older,
        newer_q: newer_cursor.as_ref().map(|c| cursor_q("after", c)),
        older_q: older_cursor.as_ref().map(|c| cursor_q("before", c)),
        position,
    };

    Ok(Ok((
        LogsSnapshot {
            me: Me::from(&user),
            queue,
            decisions,
            activity,
        },
        links,
    )))
}

/// The same read the page renders, as JSON — polled by `tests/http.rs`. The
/// newest page throughout, matching the read before pagination existed.
#[route(POST "/api/current_logs")]
async fn current_logs(cx: &Cx) -> Result<Json<std::result::Result<LogsSnapshot, Refusal>>> {
    Ok(Json(
        snapshot(
            cx,
            Section::Activity,
            FeedPage::Newest,
            Dir::Newest,
            &ActivityFilter::default(),
            LIMIT,
        )
        .await?
        .map(|(snapshot, _links)| snapshot),
    ))
}

fn query_value<'q>(query: &'q str, key: &str) -> Option<&'q str> {
    query.split('&').find_map(|pair| {
        pair.split_once('=')
            .filter(|(k, _)| *k == key)
            .map(|(_, v)| v)
    })
}

/// Which rail section the page renders. Only one is drawn at a time; a
/// section name it does not recognize falls back to `Activity`, the page's
/// namesake feed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Queue,
    Decisions,
    Activity,
}

/// The class a rail link wears: `active` on the section it points to when
/// that is the one showing, plain otherwise.
fn rail_class(current: Section, target: Section) -> &'static str {
    if current == target {
        "settings-section-link active"
    } else {
        "settings-section-link"
    }
}

#[page("/logs")]
async fn logs_page(cx: &Cx) -> Result {
    let lang = Lang::En;
    let query = topcoat::router::request::uri(cx).query().unwrap_or("").to_string();
    let section = match query_value(&query, "section") {
        Some("queue") => Section::Queue,
        Some("decisions") => Section::Decisions,
        _ => Section::Activity,
    };
    let page = parse_page(&query);
    let dir = parse_dir(&query);
    // The admin's own timezone resolves `on=` before the session is known to
    // `snapshot` itself, so a signed-out visitor still gets a clean refusal
    // rather than a second, redundant lookup here.
    let zone = match require_admin(cx).await {
        Ok(user) => izlek_core::detail::parse_zone(&user.timezone),
        Err(_) => time::UtcOffset::UTC,
    };
    let filter = parse_activity_filter(&query, zone);
    let limit = resolve_limit(cx, section);
    match snapshot(cx, section, page, dir, &filter, limit).await {
        Ok(Ok((snapshot, links))) => logs_screen(cx, snapshot, section, links, &query, limit).await,
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

/// The section name each rail link and page nav carries in the query.
fn section_slug(section: Section) -> &'static str {
    match section {
        Section::Queue => "queue",
        Section::Decisions => "decisions",
        Section::Activity => "activity",
    }
}

async fn logs_screen(
    cx: &Cx,
    snapshot: LogsSnapshot,
    section: Section,
    links: PageLinks,
    query: &str,
    limit: u32,
) -> Result {
    let lang = Lang::from_code(&snapshot.me.language);
    let me = snapshot.me;
    let queue = snapshot.queue;
    let decisions = snapshot.decisions;
    let activity = snapshot.activity;
    let queue_empty = queue.is_empty();
    let decisions_empty = decisions.is_empty();
    let activity_empty = activity.is_empty();
    let slug = section_slug(section);
    let extra = if section == Section::Activity { activity_query_suffix(query) } else { String::new() };
    let newer_href = links
        .newer_q
        .filter(|_| links.show_newer)
        .map(|q| format!("/logs?section={slug}&{q}{extra}"));
    let older_href = links
        .older_q
        .filter(|_| links.show_older)
        .map(|q| format!("/logs?section={slug}&{q}{extra}"));
    let position_note = links.position.map(|(x, y, n)| format!("{x}\u{2013}{y} / {n}"));

    let (filter_actor, filter_kind, filter_task, filter_on, filter_dir) = (
        query_value(query, "actor").unwrap_or("").to_string(),
        query_value(query, "kind").unwrap_or("").to_string(),
        query_value(query, "task").unwrap_or("").to_string(),
        query_value(query, "on").unwrap_or("").to_string(),
        query_value(query, "dir").unwrap_or("").to_string(),
    );
    let members: Vec<(String, String)> = if section == Section::Activity {
        let store = accounts(cx).store().clone();
        match store.user(&me.id).await? {
            Some(admin) => store
                .users(&admin.workspace_id)
                .await?
                .into_iter()
                .map(|u| (u.id, u.display_name))
                .collect(),
            None => Vec::new(),
        }
    } else {
        Vec::new()
    };

    view! {
        cx =>
        <header class="topbar">
            <a class="wordmark" href="/">
                <span class="wordmark-text">"izlek"</span>
                <span class="wordmark-dot"></span>
            </a>
            (crate::layout::topbar_nav(cx, crate::layout::NavPage::Logs, me.role, lang).await?)
            <div class="spacer"></div>
            (crate::layout::user_menu(cx, &me, lang).await?)
        </header>

        <div class="settings-shell">
            <nav class="settings-sections">
                <a class=(rail_class(section, Section::Queue)) href="/logs?section=queue">(t(lang, Key::MailQueue))</a>
                <a class=(rail_class(section, Section::Decisions)) href="/logs?section=decisions">(t(lang, Key::MailDecisions))</a>
                <a class=(rail_class(section, Section::Activity)) href="/logs?section=activity">(t(lang, Key::Activity))</a>
            </nav>
            <main class="settings-stage stage-wide">
                <div class="settings-head">
                    <h1 class="settings-title">(t(lang, Key::Logs))</h1>
                    <span class="chip chip-admin">(t(lang, Key::AdminOnly))</span>
                </div>

                if section == Section::Queue {
                <section class="panel">
                    <div class="panel-head">
                        <h2 class="panel-title">(t(lang, Key::MailQueue))</h2>
                    </div>
                    <div class="panel-body">
                        <div class="rule-list" data-rows=(limit) data-section="queue">
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
                    if !queue_empty {
                    <div class="panel-foot panel-foot-split">
                        <div class="foot-side">
                            if let Some(note) = position_note.clone() {
                                <span class="log-count">(note)</span>
                            }
                            if let Some(newer_href) = newer_href.clone() {
                                <a class="quiet" href=(newer_href)>(t(lang, Key::Newer))</a>
                            }
                        </div>
                        <div class="foot-side">
                            if let Some(older_href) = older_href.clone() {
                                <a class="quiet" href=(older_href)>(t(lang, Key::Older))</a>
                            }
                        </div>
                    </div>
                    }
                </section>
                }

                if section == Section::Decisions {
                <section class="panel">
                    <div class="panel-head">
                        <h2 class="panel-title">(t(lang, Key::MailDecisions))</h2>
                    </div>
                    <div class="panel-body">
                        <div class="rule-list" data-rows=(limit) data-section="decisions">
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
                    if !decisions_empty {
                    <div class="panel-foot panel-foot-split">
                        <div class="foot-side">
                            if let Some(note) = position_note.clone() {
                                <span class="log-count">(note)</span>
                            }
                            if let Some(newer_href) = newer_href.clone() {
                                <a class="quiet" href=(newer_href)>(t(lang, Key::Newer))</a>
                            }
                        </div>
                        <div class="foot-side">
                            if let Some(older_href) = older_href.clone() {
                                <a class="quiet" href=(older_href)>(t(lang, Key::Older))</a>
                            }
                        </div>
                    </div>
                    }
                </section>
                }

                if section == Section::Activity {
                <section class="panel">
                    <div class="panel-head">
                        <h2 class="panel-title">(t(lang, Key::Activity))</h2>
                    </div>
                    <div class="panel-body">
                        <form class="filterbar log-filterbar" method="get" action="/logs">
                            <input type="hidden" name="section" value="activity">
                            <select class="field-input" name="actor" data-autosubmit="">
                                <option value="" selected=(filter_actor.is_empty())>(t(lang, Key::All))</option>
                                <option value="system" selected=(filter_actor == "system")>(t(lang, Key::LogSystem))</option>
                                for member in &members {
                                    <option value=(member.0.clone()) selected=(filter_actor == member.0)>(member.1.clone())</option>
                                }
                            </select>
                            <select class="field-input" name="kind" data-autosubmit="">
                                <option value="" selected=(filter_kind.is_empty())>(t(lang, Key::All))</option>
                                for kind in ACTIVITY_KINDS {
                                    <option value=(kind) selected=(filter_kind == *kind)>(crate::i18n::activity_kind_word(lang, kind))</option>
                                }
                            </select>
                            <input class="field-input" type="text" name="task" value=(filter_task.clone()) placeholder=(t(lang, Key::Task))>
                            <div class="edit edit-pop datepick-pop">
                                <input class="edit-toggle" type="checkbox" id="log-on-toggle" aria-label=(t(lang, Key::All))>
                                <label class="field-box edit-view edit-hit" for="log-on-toggle">
                                    <span class="field-text datepick-label" data-empty=(t(lang, Key::All))>(if filter_on.is_empty() { t(lang, Key::All).to_string() } else { filter_on.clone() })</span>
                                </label>
                                <div class="edit-form pop-panel datepick-panel">
                                    (datepicker_grid(cx, "on", &filter_on, true, lang).await?)
                                </div>
                            </div>
                            <select class="field-input" name="dir" data-autosubmit="">
                                <option value="" selected=(filter_dir != "oldest")>(t(lang, Key::Newest))</option>
                                <option value="oldest" selected=(filter_dir == "oldest")>(t(lang, Key::Oldest))</option>
                            </select>
                        </form>
                        <div class="log-list" data-rows=(limit) data-section="activity">
                            for line in activity {
                                let title = line.title;
                                <div class="log-row">
                                    <span class="log-stamp">(line.at)</span>
                                    <span class="log-actor">(line.actor)</span>
                                    <span class="log-line">(line.sentence)</span>
                                    if let Some(title) = title {
                                        <span class="log-title">(title)</span>
                                    }
                                </div>
                            }
                            if activity_empty {
                                <p class="rules-quiet">(t(lang, Key::NothingYet))</p>
                            }
                        </div>
                    </div>
                    if !activity_empty {
                    <div class="panel-foot panel-foot-split">
                        <div class="foot-side">
                            if let Some(note) = position_note.clone() {
                                <span class="log-count">(note)</span>
                            }
                            if let Some(newer_href) = newer_href.clone() {
                                <a class="quiet" href=(newer_href)>(t(lang, Key::Newer))</a>
                            }
                        </div>
                        <div class="foot-side">
                            if let Some(older_href) = older_href.clone() {
                                <a class="quiet" href=(older_href)>(t(lang, Key::Older))</a>
                            }
                        </div>
                    </div>
                    }
                </section>
                }
            </main>
        </div>
        (crate::dropdown::dropdown_script(cx).await?)
        (crate::layout::escape_script(cx).await?)
        (crate::detail::datepicker_script(cx, lang).await?)
        (log_fit_script(cx).await?)
    }
}

/// Fits the active tab's page size to the browser's own viewport: measured
/// once per load/swap against the first rendered row, never against a guess
/// at the row height. A fit that would change the page size reloads once
/// through a fresh `izlek_rows_<section>` cookie; the `sessionStorage` guard,
/// keyed to the exact fit computed, stops a borderline measurement from
/// reloading forever. A container too short to measure (no rows yet, or a
/// stage not yet laid out) is left alone rather than guessed at.
async fn log_fit_script(cx: &Cx) -> Result {
    use topcoat::view::Unescaped;
    let js = "(function() {\
        var list = document.querySelector('.rule-list[data-rows], .log-list[data-rows]');\
        if (!list) { return; }\
        var section = list.dataset.section;\
        var current = parseInt(list.dataset.rows, 10);\
        var row = list.firstElementChild;\
        if (!row || !row.offsetHeight) { return; }\
        var stage = list.closest('.settings-stage');\
        if (!stage) { return; }\
        var avail = stage.getBoundingClientRect().bottom - list.getBoundingClientRect().top - 44;\
        var fit = Math.max(10, Math.floor(avail / row.offsetHeight));\
        if (fit === current) { return; }\
        var guard = 'izlekLogFit:' + section + ':' + fit;\
        if (window.sessionStorage.getItem(guard)) { return; }\
        window.sessionStorage.setItem(guard, '1');\
        document.cookie = 'izlek_rows_' + section + '=' + fit + ';path=/';\
        location.replace(location.href);\
    })();";
    view! { cx => <script>(Unescaped::new_unchecked(js))</script> }
}
