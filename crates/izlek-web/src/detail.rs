//! The task detail modal, ported from the old UI's TaskDetail
//! artboard.
//!
//! The old UI rendered this once and patched it in the browser from a resource;
//! topcoat has no browser bundle at all, so every one of the ten calls below
//! answers a plain form post with a 303 back to the page it came from —
//! [`crate::server::carry_refusal_on_redirect`] carries the refusal (if any)
//! onto that redirect's query the same way `crate::auth`'s calls do — and the
//! page it lands on reads the fresh task straight off the store. There is no
//! resource to refetch and no action to hold a pending state: a reload *is*
//! the refresh.
//!
//! Every mutating call still checks the task belongs to the asker's workspace
//! before it does anything — a task id in a form is an authorization
//! question, not a validation one — and every mail-rule wiring point
//! (`after_activity`/`after_freeing`) from the old version is carried over
//! unchanged.

use izlek_core::board::{DeadlineState, Person};
use izlek_core::detail::{
    Comment, DeletionCost, DependencyEdge, TaskDetail, TaskFacts, moment_label_in, parse_zone,
};
use izlek_core::store::{Store, StoreError, User};
use serde::{Deserialize, Serialize};
use time::{Date, UtcOffset};
use topcoat::Result;
use topcoat::context::Cx;
use topcoat::router::content::{Form, Json};
use topcoat::router::{HeaderName, StatusCode, header, route};
use topcoat::view::{class, view};

use crate::i18n::{Key, Lang, t};
use crate::server::{Refusal, accounts, back_to, mail, refusal_of, require_user, require_writer};

/// A task this board could be linked to: enough to name it in the picker and
/// nothing more.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkTarget {
    pub id: String,
    pub task_key: String,
    pub title: String,
}

/// The person the current browser is signed in as, wired the same as
/// `izlek-web/src/auth.rs`'s `Me` so [`fetch_task`]'s answer keeps its shape.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Me {
    pub id: String,
    pub display_name: String,
    pub email: String,
    pub role: izlek_core::Role,
    pub language: String,
    pub has_photo: bool,
}

impl From<&User> for Me {
    fn from(user: &User) -> Self {
        Me {
            id: user.id.clone(),
            display_name: user.display_name.clone(),
            email: user.email.clone(),
            role: user.role,
            language: user.language.clone(),
            has_photo: user.has_photo,
        }
    }
}

/// One task detail's worth of state — the [`fetch_task`] answer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DetailSnapshot {
    pub detail: TaskDetail,
    pub me: Me,
    pub today: Date,
    /// The viewer's stored display timezone — comments and activity stamps
    /// shift into this, the same as `/logs` does.
    pub zone: UtcOffset,
    pub linkable: Vec<LinkTarget>,
    /// Tasks this one could take in as parts: the board's other top-level
    /// tasks that have no parts of their own. Empty when this task is itself
    /// a part — subtasks go one level deep.
    pub adoptable: Vec<LinkTarget>,
    pub may_write: bool,
    pub may_comment: bool,
    pub may_delete: bool,
    pub allowed_file_types: Vec<String>,
    pub attachment_limit_mb: u64,
    pub notifications: Vec<NotificationLine>,
    pub may_administer: bool,
    /// Every tag on this workspace's board, in the admin's hand-set order —
    /// the tag field reads both its options and its current value from here.
    pub tags: Vec<izlek_core::store::Tag>,
}

/// One send's fate, joined to the sends of its decision's event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendLine {
    pub recipient: String,
    pub state: String,
    /// The chip's tone — see `logs.rs`'s `QueueLine::state_kind`.
    pub state_kind: String,
    pub attempts: u32,
    /// What the mail server said when it refused, and `None` for anyone who
    /// may not administer — the same gate `rule_name` gets, for the same
    /// reason. Which people a task mailed, and whether it arrived, is a fact
    /// about the task and belongs to everyone who can read it. *Why the mail
    /// server refused* is a fact about the workspace's sender — a host, an
    /// account, a rejected credential — and this app keeps that admin-only
    /// everywhere else (`/api/current_logs` is admin-gated; the live channel
    /// will not even name `Topic::Queue` to a member). Gated here, where the
    /// line is built, so no view can print what it was never given.
    pub last_error: Option<String>,
    pub sent_at: Option<String>,
}

/// One decision the task's rules made about one event, with what happened to
/// any mail it owed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationLine {
    /// What caused it — "moved to Done" — absent when the event is gone.
    pub happened: Option<String>,
    pub at: String,
    /// Shown only to an admin.
    pub rule_name: Option<String>,
    pub outcome: String,
    pub outcome_kind: String,
    pub sends: Vec<SendLine>,
}

/// Which way round a dependency runs, as it travels in a form.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    BlockedBy,
    Blocks,
}

/// Which task is `blocked` and which is `blocking`, from the task the modal
/// is open on, the other task it names, and which way the form said the link
/// runs.
fn resolve_direction(task_id: &str, other_id: &str, direction: Direction) -> (String, String) {
    match direction {
        Direction::BlockedBy => (task_id.to_string(), other_id.to_string()),
        Direction::Blocks => (other_id.to_string(), task_id.to_string()),
    }
}

#[cfg(test)]
mod resolve_direction_tests {
    use super::{Direction, resolve_direction};

    #[test]
    fn blocked_by_puts_this_task_first() {
        assert_eq!(
            resolve_direction("this", "other", Direction::BlockedBy),
            ("this".to_string(), "other".to_string())
        );
    }

    #[test]
    fn blocks_puts_the_other_task_first() {
        assert_eq!(
            resolve_direction("this", "other", Direction::Blocks),
            ("other".to_string(), "this".to_string())
        );
    }
}

// -- guards -------------------------------------------------------------

/// The task, if this person's workspace is the one holding it. A task in
/// another workspace is not found rather than forbidden: the answer says
/// nothing about whether the id is real.
async fn task_of(
    store: &dyn Store,
    user: &User,
    task_id: &str,
) -> std::result::Result<TaskFacts, Refusal> {
    match store.task(task_id).await {
        Ok(Some(facts)) if facts.workspace_id == user.workspace_id => Ok(facts),
        Ok(_) => Err(Refusal::NotFound),
        Err(error) => {
            eprintln!("store error: {error}");
            Err(Refusal::Unavailable)
        }
    }
}

/// The writer behind this request and the task they named.
async fn writer_and_task(
    cx: &Cx,
    task_id: &str,
) -> std::result::Result<(User, TaskFacts), Refusal> {
    let user = require_writer(cx).await?;
    let store = accounts(cx).store().clone();
    let facts = task_of(store.as_ref(), &user, task_id).await?;
    Ok((user, facts))
}

/// A 303 back to wherever the form was posted from, carrying `refusal` as the
/// body for `carry_refusal_on_redirect` to read — the same shape `auth.rs`
/// uses for every one of its mutating calls.
type Redirect = Result<(StatusCode, [(HeaderName, String); 1], Json<Option<Refusal>>)>;

fn redirect(cx: &Cx, refusal: Option<Refusal>) -> Redirect {
    Ok((
        StatusCode::SEE_OTHER,
        [(header::LOCATION, back_to(cx, "/"))],
        Json(refusal),
    ))
}

// -- the ten calls --------------------------------------------------------

#[derive(Deserialize)]
struct TaskIdForm {
    task_id: String,
}

#[derive(Deserialize)]
struct SaveTaskForm {
    task_id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    deadline: Option<String>,
    #[serde(default)]
    clock_hour: Option<String>,
    #[serde(default)]
    clock_minute: Option<String>,
}

#[derive(Deserialize)]
struct PersonForm {
    task_id: String,
    user_id: String,
}

#[derive(Deserialize)]
struct LinkForm {
    task_id: String,
    other_id: String,
    direction: Direction,
}

#[derive(Deserialize)]
struct NewSubtaskForm {
    parent_id: String,
    title: String,
}

#[derive(Deserialize)]
struct ParentForm {
    task_id: String,
    /// Empty means "let it out": the same form both ways, because parenting
    /// and promoting are the same single write.
    #[serde(default)]
    parent_id: String,
}

#[derive(Deserialize)]
struct CommentForm {
    task_id: String,
    body: String,
}

#[derive(Deserialize)]
struct FileIdForm {
    file_id: String,
}

/// Everything one task detail shows. A Viewer may read it; what they may not
/// do is refused in the calls below, not merely hidden from them here.
///
/// Shared by the route and [`task_modal`], which calls it directly rather
/// than going back through its own HTTP endpoint — the same shape `pages.rs`
/// reads the store straight through for its own screens.
async fn load_snapshot(
    cx: &Cx,
    task_id: &str,
) -> Result<std::result::Result<DetailSnapshot, Refusal>> {
    use izlek_core::detail::load;
    use time::OffsetDateTime;

    let user = match require_user(cx).await {
        Ok(user) => user,
        Err(refusal) => return Ok(Err(refusal)),
    };
    let store = accounts(cx).store().clone();

    let Some(mut detail) = load(store.as_ref(), &user.workspace_id, task_id).await? else {
        return Ok(Err(Refusal::NotFound));
    };

    // What the picker may offer: this board's other tasks, minus the ones
    // already on either end of a live link with this one. A cleared edge is
    // not a link any more, so its task comes back to the picker.
    let mut linkable = Vec::new();
    let mut adoptable = Vec::new();
    if let Some(board) = store.board(&user.workspace_id).await? {
        let taken: Vec<&str> = detail
            .blocked_by
            .iter()
            .chain(detail.blocks.iter())
            .filter(|edge| edge.cleared_at.is_none())
            .map(|edge| edge.task_id.as_str())
            .collect();
        let tasks = store.tasks_for_board(&board.id).await?;
        // Who already has parts, so the picker does not offer a task whose
        // own children would become grandchildren.
        let parents: Vec<&str> = tasks
            .iter()
            .filter_map(|task| task.parent_id.as_deref())
            .collect();
        for task in &tasks {
            let is_self = task.id == detail.id;
            if detail.parent.is_none()
                && !is_self
                && task.parent_id.is_none()
                && !parents.contains(&task.id.as_str())
            {
                adoptable.push(LinkTarget {
                    id: task.id.clone(),
                    task_key: task.task_key.clone(),
                    title: task.title.clone(),
                });
            }
        }
        // A task's own family is not offered: a parent and its part are
        // already related, the store refuses the edge, and a picker that
        // offers what will always be refused is a worse control than one that
        // does not offer it.
        let family: Vec<&str> = detail
            .subtasks
            .iter()
            .map(|part| part.id.as_str())
            .chain(detail.parent.iter().map(|whole| whole.id.as_str()))
            .collect();
        for task in tasks {
            if task.id == detail.id
                || taken.contains(&task.id.as_str())
                || family.contains(&task.id.as_str())
            {
                continue;
            }
            linkable.push(LinkTarget {
                id: task.id,
                task_key: task.task_key,
                title: task.title,
            });
        }
        adoptable.sort_by(|a, b| a.id.cmp(&b.id));
        // Sorted by id, not `task_key`: the key's tail is a random ULID
        // suffix now, not a counter, so only the id (a ULID itself) still
        // orders these by creation time.
        linkable.sort_by(|a, b| a.id.cmp(&b.id));
    }

    // The tag field's options: every tag on the board, in the admin's
    // hand-set order. A workspace with no board yet has no tags either.
    let tags = match store.board(&user.workspace_id).await? {
        Some(board) => store.tags(&board.id).await?,
        None => Vec::new(),
    };

    let may_write = user.role.can_write_tasks();
    let may_administer = user.role.can_administer();
    let lang = Lang::from_code(&user.language);
    let zone = parse_zone(&user.timezone);
    // Column names are stored data; the task's own status and the move
    // picker's choices read in the viewer's language.
    detail.column.name = crate::i18n::column_name(lang, &detail.column.name);
    for column in &mut detail.columns {
        column.name = crate::i18n::column_name(lang, &column.name);
    }

    let decisions = store.decisions_for_task(&detail.id, 10).await?;
    let sends = store.sends_for_task(&detail.id, 50).await?;
    let mut columns_cache: std::collections::HashMap<String, Vec<izlek_core::board::Column>> =
        std::collections::HashMap::new();
    let mut notifications = Vec::with_capacity(decisions.len());
    for decision in decisions {
        let rule = store.mail_rule(&decision.rule_id).await?;
        let rule_name = may_administer.then(|| {
            rule.as_ref()
                .map(|rule| rule.subject.clone())
                .unwrap_or_else(|| t(lang, Key::RuleGone).to_string())
        });
        let outcome_detail = crate::logs::decision_detail(
            &store,
            &mut columns_cache,
            rule.as_ref().map(|rule| rule.board_id.as_str()),
            decision.outcome,
            &decision.detail,
            lang,
        )
        .await?;
        let happened = crate::logs::event_happened(&store, &decision.event_id, lang).await?;
        let matching: Vec<SendLine> = sends
            .iter()
            .filter(|s| s.event_id.as_deref() == Some(decision.event_id.as_str()))
            .map(|send| {
                use izlek_core::store::SendState;
                let (state, state_kind) = match send.state {
                    SendState::Pending => (t(lang, Key::QueueStatePending), "pending"),
                    SendState::Failed if send.attempts == 0 => {
                        (t(lang, Key::QueueStateHeld), "held")
                    }
                    SendState::Failed => (t(lang, Key::QueueStateFailed), "failed"),
                    SendState::Sent => (t(lang, Key::QueueStateSent), "sent"),
                    SendState::Abandoned => (t(lang, Key::QueueStateAbandoned), "abandoned"),
                };
                SendLine {
                    recipient: send.recipient.clone(),
                    state: state.to_string(),
                    state_kind: state_kind.to_string(),
                    attempts: send.attempts,
                    last_error: may_administer.then(|| send.last_error.clone()).flatten(),
                    sent_at: send.sent_at.map(|at| moment_label_in(at, zone)),
                }
            })
            .collect();
        let outcome = crate::logs::outcome_word(decision.outcome, lang).to_string();
        notifications.push(NotificationLine {
            happened,
            at: moment_label_in(decision.at, zone),
            rule_name,
            outcome: if outcome_detail.is_empty() {
                outcome
            } else {
                format!("{outcome} · {outcome_detail}")
            },
            outcome_kind: decision.outcome.as_str().to_string(),
            sends: matching,
        });
    }

    const MB: u64 = 1024 * 1024;
    let workspace = store.workspace().await?;
    let allowed_file_types = workspace
        .as_ref()
        .map(|workspace| workspace.allowed_file_types.clone())
        .unwrap_or_default();
    let attachment_limit_mb = workspace
        .map(|workspace| workspace.attachment_limit_bytes / MB)
        .unwrap_or(0);

    Ok(Ok(DetailSnapshot {
        detail,
        linkable,
        adoptable,
        may_write,
        may_comment: user.role.can_comment(),
        may_delete: may_write,
        allowed_file_types,
        attachment_limit_mb,
        notifications,
        tags,
        may_administer,
        me: Me::from(&user),
        today: OffsetDateTime::now_utc().date(),
        zone: parse_zone(&user.timezone),
    }))
}

#[route(POST "/api/fetch_task")]
async fn fetch_task(
    cx: &Cx,
    Form(input): Form<TaskIdForm>,
) -> Result<Json<std::result::Result<DetailSnapshot, Refusal>>> {
    Ok(Json(load_snapshot(cx, &input.task_id).await?))
}

/// One moment field, two posted words: the day the grid carries and an
/// optional 24h `HH:MM`. Each word keeps its own contract. The day: absent
/// keeps what the task already says, `""` clears it, a date sets it. The
/// time: absent says nothing at all (the clock stays, whoever posted the
/// form without it — a title edit), `""` clears the clock, a value makes
/// the clock on the resolvable day (the posted one, or the task's own) and
/// writes that day back so the two columns agree. A time with no day
/// anywhere, or one that does not parse, is refused.
pub(crate) fn moment_field(
    day_raw: Option<&str>,
    existing: (Option<Date>, Option<time::OffsetDateTime>),
    time_raw: Option<&str>,
    zone: UtcOffset,
) -> std::result::Result<(Option<Date>, Option<time::OffsetDateTime>), Refusal> {
    use time::macros::format_description;

    let (existing_day, existing_clock) = existing;
    let day = match day_raw {
        None => existing_day,
        Some(raw) => match raw.trim() {
            "" => None,
            value => Some(
                Date::parse(value, format_description!("[year]-[month]-[day]"))
                    .map_err(|_| Refusal::BadDeadline)?,
            ),
        },
    };
    let Some(time) = time_raw else {
        return Ok((day, existing_clock));
    };
    let time = time.trim();
    if time.is_empty() {
        return Ok((day, None));
    }
    let Some(day) = day else {
        return Err(Refusal::BadClock);
    };
    let Some((hour, minute)) = parse_hhmm(time) else {
        return Err(Refusal::BadClock);
    };
    let local = day
        .with_hms(hour, minute, 0)
        .map_err(|_| Refusal::BadClock)?
        .assume_offset(zone)
        .to_offset(UtcOffset::UTC);
    // The clock speaks for the day it sits on: the deadline day and the
    // moment's day are one fact, whatever the grid last showed.
    Ok((Some(day), Some(local)))
}

/// `H:MM` or `HH:MM`, 24h, real minutes — the only shape the moment field's
/// time box accepts.
fn parse_hhmm(raw: &str) -> Option<(u8, u8)> {
    let (hour, minute) = raw.split_once(':')?;
    if minute.len() != 2 {
        return None;
    }
    let hour: u8 = hour.parse().ok()?;
    let minute: u8 = minute.parse().ok()?;
    (hour <= 23 && minute <= 59).then_some((hour, minute))
}

/// The posted hour and minute words as one `HH:MM` for [`moment_field`],
/// which stays the one validator of the shape. Both absent: the form said
/// nothing about the time. Both empty: the popover's Clear, clearing the
/// clock. Both set: the moment, however the hands typed it. One without
/// the other is a form gone wrong, not something to guess past.
pub(crate) fn combine_clock(
    hour: Option<&str>,
    minute: Option<&str>,
) -> std::result::Result<Option<String>, Refusal> {
    match (hour, minute) {
        (None, None) => Ok(None),
        (Some(""), Some("")) => Ok(Some(String::new())),
        (Some(hour), Some(minute)) => {
            Ok(Some(format!("{}:{}", hour.trim(), minute.trim())))
        }
        _ => Err(Refusal::BadClock),
    }
}

/// Saves the title, the description and the moment field — the deadline day
/// with its optional time. Status is not here: the column a task sits in is
/// changed by moving it, a call `board.rs` owns.
#[route(POST "/api/save_task")]
async fn save_task(cx: &Cx, Form(input): Form<SaveTaskForm>) -> Redirect {
    use time::OffsetDateTime;

    let (user, facts) = match writer_and_task(cx, &input.task_id).await {
        Ok(pair) => pair,
        Err(refusal) => return redirect(cx, Some(refusal)),
    };

    let title = match input.title {
        Some(given) => {
            let trimmed = given.trim().to_string();
            if trimmed.is_empty() {
                return redirect(cx, Some(Refusal::EmptyTitle));
            }
            trimmed
        }
        None => facts.row.title.clone(),
    };
    let description = match &input.description {
        Some(given) => given.trim().to_string(),
        None => facts.description.clone(),
    };
    let zone = parse_zone(&user.timezone);
    let time = match combine_clock(input.clock_hour.as_deref(), input.clock_minute.as_deref()) {
        Ok(time) => time,
        Err(refusal) => return redirect(cx, Some(refusal)),
    };
    let (deadline, clock_at) = match moment_field(
        input.deadline.as_deref(),
        (facts.row.deadline, facts.row.clock_at),
        time.as_deref(),
        zone,
    ) {
        Ok(pair) => pair,
        Err(refusal) => return redirect(cx, Some(refusal)),
    };

    let store = accounts(cx).store().clone();
    let activity_ids = store
        .save_task(
            &input.task_id,
            &title,
            &description,
            deadline,
            clock_at,
            &user.id,
            OffsetDateTime::now_utc(),
        )
        .await?;
    for activity_id in activity_ids {
        mail(cx).after_activity(store.clone(), activity_id);
    }
    redirect(cx, None)
}

#[derive(Deserialize)]
struct SetTaskTagForm {
    task_id: String,
    tag_id: String,
}

/// Files a task under a tag. A Viewer can read tags but not set them; the
/// tag has to be this workspace board's own. The relabeling lands on the
/// task's own trail and the workspace feed; no rule mails about it, since no
/// trigger matches a tag.
#[route(POST "/api/set_task_tag")]
async fn set_task_tag(cx: &Cx, Form(input): Form<SetTaskTagForm>) -> Redirect {
    let (actor, _) = match writer_and_task(cx, &input.task_id).await {
        Ok(pair) => pair,
        Err(refusal) => return redirect(cx, Some(refusal)),
    };
    let store = accounts(cx).store().clone();
    let Some(board) = store.board(&actor.workspace_id).await? else {
        return redirect(cx, Some(Refusal::Unavailable));
    };
    let tags = store.tags(&board.id).await?;
    let Some(tag) = tags.iter().find(|tag| tag.id == input.tag_id) else {
        return redirect(cx, Some(Refusal::NotFound));
    };
    store.set_task_tag(&input.task_id, &input.tag_id).await?;
    let _ = store
        .record_activity(
            &input.task_id,
            Some(&actor.id),
            None,
            &izlek_core::detail::ActivityKind::Other("tagged".to_string()),
            &tag.name,
            time::OffsetDateTime::now_utc(),
        )
        .await;
    redirect(cx, None)
}

/// Puts someone on a task. A Viewer can neither do this nor be the target.
#[route(POST "/api/assign")]
async fn assign(cx: &Cx, Form(input): Form<PersonForm>) -> Redirect {
    use izlek_core::detail::ActivityKind;
    use time::OffsetDateTime;

    let (actor, _) = match writer_and_task(cx, &input.task_id).await {
        Ok(pair) => pair,
        Err(refusal) => return redirect(cx, Some(refusal)),
    };
    let store = accounts(cx).store().clone();

    let Some(person) = store.user(&input.user_id).await? else {
        return redirect(cx, Some(Refusal::NotFound));
    };
    if person.workspace_id != actor.workspace_id {
        return redirect(cx, Some(Refusal::NotFound));
    }
    if !person.role.can_be_assigned() {
        return redirect(cx, Some(Refusal::Forbidden));
    }

    store.assign_task(&input.task_id, &person.id).await?;
    let activity_id = store
        .record_activity(
            &input.task_id,
            Some(&actor.id),
            Some(&person.id),
            &ActivityKind::Assigned,
            &person.display_name,
            OffsetDateTime::now_utc(),
        )
        .await?;
    mail(cx).after_activity(store, activity_id);
    redirect(cx, None)
}

#[route(POST "/api/unassign")]
async fn unassign(cx: &Cx, Form(input): Form<PersonForm>) -> Redirect {
    use izlek_core::detail::ActivityKind;
    use time::OffsetDateTime;

    let (actor, _) = match writer_and_task(cx, &input.task_id).await {
        Ok(pair) => pair,
        Err(refusal) => return redirect(cx, Some(refusal)),
    };
    let store = accounts(cx).store().clone();

    let Some(person) = store.user(&input.user_id).await? else {
        return redirect(cx, Some(Refusal::NotFound));
    };
    if person.workspace_id != actor.workspace_id {
        return redirect(cx, Some(Refusal::NotFound));
    }
    store.unassign_task(&input.task_id, &person.id).await?;
    let activity_id = store
        .record_activity(
            &input.task_id,
            Some(&actor.id),
            Some(&person.id),
            &ActivityKind::Unassigned,
            &person.display_name,
            OffsetDateTime::now_utc(),
        )
        .await?;
    mail(cx).after_activity(store, activity_id);
    redirect(cx, None)
}

/// Links two tasks. Both ends are checked against the asker's workspace, and
/// a link that would close a circle is refused by the store, inside the
/// transaction that would have written it.
#[route(POST "/api/link_tasks")]
async fn link_tasks(cx: &Cx, Form(input): Form<LinkForm>) -> Redirect {
    use izlek_core::detail::ActivityKind;
    use time::OffsetDateTime;

    let (actor, _) = match writer_and_task(cx, &input.task_id).await {
        Ok(pair) => pair,
        Err(refusal) => return redirect(cx, Some(refusal)),
    };
    let store = accounts(cx).store().clone();
    let other = match task_of(store.as_ref(), &actor, &input.other_id).await {
        Ok(facts) => facts,
        Err(refusal) => return redirect(cx, Some(refusal)),
    };

    let (blocked, blocking) = resolve_direction(&input.task_id, &input.other_id, input.direction);
    let now = OffsetDateTime::now_utc();
    match store.add_dependency(&blocked, &blocking, now).await {
        Ok(()) => {}
        Err(StoreError::Cycle) => return redirect(cx, Some(Refusal::Cycle)),
        Err(error) => return Err(error.into()),
    }
    let activity_id = store
        .record_activity(
            &input.task_id,
            Some(&actor.id),
            None,
            &ActivityKind::Linked,
            &other.row.task_key,
            now,
        )
        .await?;
    mail(cx).after_activity(store, activity_id);
    redirect(cx, None)
}

/// Clears a link. The row stays, marked cleared, so the history — and the
/// rules engine — still has something to read.
#[route(POST "/api/unlink_tasks")]
async fn unlink_tasks(cx: &Cx, Form(input): Form<LinkForm>) -> Redirect {
    use izlek_core::detail::ActivityKind;
    use time::OffsetDateTime;

    let (actor, _) = match writer_and_task(cx, &input.task_id).await {
        Ok(pair) => pair,
        Err(refusal) => return redirect(cx, Some(refusal)),
    };
    let store = accounts(cx).store().clone();
    let other = match task_of(store.as_ref(), &actor, &input.other_id).await {
        Ok(facts) => facts,
        Err(refusal) => return redirect(cx, Some(refusal)),
    };

    let (blocked, blocking) = resolve_direction(&input.task_id, &input.other_id, input.direction);
    let now = OffsetDateTime::now_utc();
    store.clear_dependency(&blocked, &blocking, now).await?;
    let activity_id = store
        .record_activity(
            &input.task_id,
            Some(&actor.id),
            None,
            &ActivityKind::Unlinked,
            &other.row.task_key,
            now,
        )
        .await?;
    mail(cx).after_activity(store, activity_id);
    redirect(cx, None)
}

/// Opens a new task already filed under this one. It starts in the board's
/// first column, the way a card dropped on the board's first column would.
#[route(POST "/api/create_subtask")]
async fn create_subtask(cx: &Cx, Form(input): Form<NewSubtaskForm>) -> Redirect {
    use izlek_core::store::NewTask;

    let (actor, parent) = match writer_and_task(cx, &input.parent_id).await {
        Ok(pair) => pair,
        Err(refusal) => return redirect(cx, Some(refusal)),
    };
    let title = input.title.trim();
    if title.is_empty() {
        return redirect(cx, Some(Refusal::EmptyTitle));
    }
    let store = accounts(cx).store().clone();
    let columns = store.columns(&parent.board_id).await?;
    let Some(first) = columns.iter().min_by_key(|column| column.position) else {
        return redirect(cx, Some(Refusal::NotFound));
    };
    let created = match store
        .create_task(NewTask {
            board_id: &parent.board_id,
            column_id: &first.id,
            parent_id: Some(&input.parent_id),
            title,
            description: "",
            deadline: None,
            clock_at: None,
            created_by: &actor.id,
        })
        .await
    {
        Ok(created) => created,
        Err(StoreError::NotNestable) => return redirect(cx, Some(Refusal::NotNestable)),
        Err(error) => return Err(error.into()),
    };
    mail(cx).after_activity(store.clone(), created.activity_id);
    mail(cx).after(created.transition);
    redirect(cx, None)
}

/// Files a task under another one, or lets it out again — an empty
/// `parent_id` is the second. One handler, because it is one write.
#[route(POST "/api/set_parent")]
async fn set_parent(cx: &Cx, Form(input): Form<ParentForm>) -> Redirect {
    use izlek_core::detail::ActivityKind;
    use time::OffsetDateTime;

    let (actor, task) = match writer_and_task(cx, &input.task_id).await {
        Ok(pair) => pair,
        Err(refusal) => return redirect(cx, Some(refusal)),
    };
    let store = accounts(cx).store().clone();
    let wanted = input.parent_id.trim();

    // Whichever end is not the task itself has to be a task this person may
    // see, so the key the activity line names is one they already have.
    let (parent_id, other_key) = if wanted.is_empty() {
        let Some(had) = task.row.parent_id.clone() else {
            return redirect(cx, None);
        };
        match task_of(store.as_ref(), &actor, &had).await {
            Ok(facts) => (None, facts.row.task_key),
            Err(refusal) => return redirect(cx, Some(refusal)),
        }
    } else {
        match task_of(store.as_ref(), &actor, wanted).await {
            Ok(facts) => (Some(facts.row.id.clone()), facts.row.task_key),
            Err(refusal) => return redirect(cx, Some(refusal)),
        }
    };

    match store.set_parent(&input.task_id, parent_id.as_deref()).await {
        Ok(()) => {}
        Err(StoreError::NotNestable) => return redirect(cx, Some(Refusal::NotNestable)),
        Err(StoreError::OtherBoard) => return redirect(cx, Some(Refusal::Forbidden)),
        Err(StoreError::Cycle) => return redirect(cx, Some(Refusal::Cycle)),
        Err(StoreError::NotFound) => return redirect(cx, Some(Refusal::NotFound)),
        Err(error) => return Err(error.into()),
    }

    let kind = if parent_id.is_some() {
        ActivityKind::Parented
    } else {
        ActivityKind::Unparented
    };
    let activity_id = store
        .record_activity(
            &input.task_id,
            Some(&actor.id),
            None,
            &kind,
            &other_key,
            OffsetDateTime::now_utc(),
        )
        .await?;
    mail(cx).after_activity(store, activity_id);
    redirect(cx, None)
}

/// Writes a comment. The author is the session's user; there is no author
/// field on the form. A Viewer is refused here, not merely shown no textarea.
#[route(POST "/api/post_comment")]
async fn post_comment(cx: &Cx, Form(input): Form<CommentForm>) -> Redirect {
    use time::OffsetDateTime;

    let user = match require_user(cx).await {
        Ok(user) => user,
        Err(refusal) => return redirect(cx, Some(refusal)),
    };
    if !user.role.can_comment() {
        return redirect(cx, Some(Refusal::Forbidden));
    }
    let store = accounts(cx).store().clone();
    if let Err(refusal) = task_of(store.as_ref(), &user, &input.task_id).await {
        return redirect(cx, Some(refusal));
    }
    let body = input.body.trim();
    if body.is_empty() {
        return redirect(cx, Some(Refusal::EmptyComment));
    }

    let written = store
        .add_comment(&input.task_id, &user.id, body, OffsetDateTime::now_utc())
        .await?;
    mail(cx).after_activity(store, written.activity_id);
    redirect(cx, None)
}

/// What a delete would take with it, for the confirmation step. Reads only.
#[route(POST "/api/what_delete_costs")]
async fn what_delete_costs(
    cx: &Cx,
    Form(input): Form<TaskIdForm>,
) -> Result<Json<std::result::Result<DeletionCost, Refusal>>> {
    if let Err(refusal) = writer_and_task(cx, &input.task_id).await {
        return Ok(Json(Err(refusal)));
    }
    match accounts(cx).store().deletion_cost(&input.task_id).await? {
        Some(cost) => Ok(Json(Ok(cost))),
        None => Ok(Json(Err(Refusal::NotFound))),
    }
}

/// Deletes a task. A writer may: the delete is soft, so a mistake is
/// recoverable by hand. Whatever was waiting only on it becomes unblocked,
/// the store records that as an event, and the unblocked rules fire on it.
#[route(POST "/api/delete_task")]
async fn delete_task(cx: &Cx, Form(input): Form<TaskIdForm>) -> Redirect {
    use time::OffsetDateTime;

    let (user, _) = match writer_and_task(cx, &input.task_id).await {
        Ok(pair) => pair,
        Err(refusal) => return redirect(cx, Some(refusal)),
    };
    let store = accounts(cx).store().clone();
    let deletion = store
        .delete_task(&input.task_id, &user.id, OffsetDateTime::now_utc())
        .await?;
    // A blocker being deleted unblocks whatever was waiting only on it, which
    // is the same news as the blocker finishing. The freeing is committed;
    // the send is a separate step, off the request.
    if let Some(freeing) = deletion.event {
        mail(cx).after_freeing(freeing, deletion.freed);
    }
    mail(cx).after_activity(store, deletion.activity_id);
    // Deleted; the referring `/?task=<id>` no longer names anything, so land
    // on the board itself rather than reopening a dead modal.
    Ok((
        StatusCode::SEE_OTHER,
        [(header::LOCATION, "/".to_string())],
        Json(None),
    ))
}

/// Deletes an attachment's row and its bytes. A hard delete, unlike
/// [`delete_task`]'s soft one — a file is a blob in the database, not a fact
/// worth an audit trail. Only the person who put it there, or an admin, may
/// take it away.
#[route(POST "/api/delete_file")]
async fn delete_file(cx: &Cx, Form(input): Form<FileIdForm>) -> Redirect {
    let user = match require_user(cx).await {
        Ok(user) => user,
        Err(refusal) => return redirect(cx, Some(refusal)),
    };
    let store = accounts(cx).store().clone();
    let Some(attachment) = store.attachment(&input.file_id).await? else {
        return redirect(cx, Some(Refusal::NotFound));
    };
    if let Err(refusal) = task_of(store.as_ref(), &user, &attachment.task_id).await {
        return redirect(cx, Some(refusal));
    }
    if user.id != attachment.uploaded_by && !user.role.can_administer() {
        return redirect(cx, Some(Refusal::Forbidden));
    }

    store.delete_attachment(&input.file_id).await?;
    let _ = store
        .record_activity(
            &attachment.task_id,
            Some(&user.id),
            None,
            &izlek_core::detail::ActivityKind::FileRemoved,
            &attachment.file_name,
            time::OffsetDateTime::now_utc(),
        )
        .await;
    redirect(cx, None)
}

// -- the screen -------------------------------------------------------------
//
// Wasm-escapes from the old version, each simplified to a plain form post
// rather than ported to a runtime signal (noted at each site below):
//
// - StatusControl's and the file input's `on:change` auto-submit are the
//   `data-autosubmit` attribute (and the file input's own class), read by
//   `layout.rs`'s `soft_nav_script` through one delegated `change`
//   listener — a per-element handler would die when a soft submit swaps
//   the page in.
// - The modal scrim's click-to-close is `soft_nav_script`'s delegated
//   click listener (a click whose target is the scrim itself closes; a
//   click inside `.modal` never has the scrim as target, so no
//   stop-propagation is needed). The X glyph and the footer "Close" link
//   route through the same soft close.
// - DeadlineControl's hand-built calendar grid (`js_sys::Date` for "today",
//   month navigation, per-day buttons) is replaced with a native
//   `<input type="date">` inside the same CSS-only edit-toggle popover —
//   the toggle itself needed no script in the old version either.
// - The two-step delete confirmation (an `ask` action fetching the cost,
//   then a second click) is collapsed into one eager read: the cost is
//   computed while the screen renders, and a native `<details>` disclosure —
//   no script — holds the confirmation and the real delete button.

/// The artboard's glyphs, drawn rather than typed — copied verbatim from the
/// old UI's `detail.rs` `glyph` module. A missing character in a font is a
/// hole in the design; an inline path is the same shape everywhere.
pub(crate) mod glyph {
    use topcoat::Result;
    use topcoat::context::Cx;
    use topcoat::view::view;

    pub async fn chevron(cx: &Cx) -> Result {
        view! {
            cx =>
            <svg class="glyph" width="14" height="14" viewBox="0 0 16 16" fill="none"
                stroke="currentColor" stroke-width="1.5" stroke-linecap="round"
                stroke-linejoin="round" aria-hidden="true">
                <path d="M4 6l4 4 4-4"></path>
            </svg>
        }
    }

    pub async fn calendar(cx: &Cx) -> Result {
        view! {
            cx =>
            <svg class="glyph" width="13" height="13" viewBox="0 0 16 16" fill="none"
                stroke="currentColor" stroke-width="1.5" stroke-linecap="round" aria-hidden="true">
                <rect x="2.5" y="3.5" width="11" height="10" rx="1.5"></rect>
                <path d="M2.5 6.5h11M5.5 2v2.5M10.5 2v2.5"></path>
            </svg>
        }
    }

    pub async fn plus(cx: &Cx) -> Result {
        view! {
            cx =>
            <svg class="glyph" width="12" height="12" viewBox="0 0 16 16" fill="none"
                stroke="currentColor" stroke-width="1.6" stroke-linecap="round" aria-hidden="true">
                <path d="M8 3v10M3 8h10"></path>
            </svg>
        }
    }

    pub async fn play(cx: &Cx) -> Result {
        view! {
            cx =>
            <svg class="glyph glyph-play" width="14" height="14" viewBox="0 0 16 16"
                fill="currentColor" stroke="currentColor" stroke-width="1.5"
                stroke-linejoin="round" aria-hidden="true">
                <path d="M5.5 3.5v9l7.5-4.5z"></path>
            </svg>
        }
    }

    pub async fn pause(cx: &Cx) -> Result {
        view! {
            cx =>
            <svg class="glyph glyph-pause" width="14" height="14" viewBox="0 0 16 16" fill="none"
                stroke="currentColor" stroke-width="2" stroke-linecap="round" aria-hidden="true">
                <path d="M5.5 3.5v9M10.5 3.5v9"></path>
            </svg>
        }
    }

    pub async fn cross(cx: &Cx) -> Result {
        view! {
            cx =>
            <svg class="glyph" width="13" height="13" viewBox="0 0 16 16" fill="none"
                stroke="currentColor" stroke-width="1.5" stroke-linecap="round" aria-hidden="true">
                <path d="M4 4l8 8M12 4l-8 8"></path>
            </svg>
        }
    }

    /// An open part. Deliberately not the padlock: a dependency row's lock
    /// means "something is in front of this", and a subtask that is simply
    /// unfinished is not blocked by anything.
    pub async fn ring(cx: &Cx) -> Result {
        view! {
            cx =>
            <svg class="glyph" width="12" height="12" viewBox="0 0 16 16" fill="none"
                stroke="currentColor" stroke-width="1.6" aria-hidden="true">
                <circle cx="8" cy="8" r="5"></circle>
            </svg>
        }
    }

    pub async fn tick(cx: &Cx) -> Result {
        view! {
            cx =>
            <svg class="glyph dep-tick" width="14" height="14" viewBox="0 0 16 16" fill="none"
                stroke="currentColor" stroke-width="1.8" stroke-linecap="round"
                stroke-linejoin="round" aria-hidden="true">
                <path d="M3 8.5l3.5 3.5L13 5"></path>
            </svg>
        }
    }

    pub async fn lock(cx: &Cx) -> Result {
        view! {
            cx =>
            <svg class="glyph" width="14" height="14" viewBox="0 0 16 16" fill="none"
                stroke="currentColor" stroke-width="1.6" stroke-linecap="round"
                stroke-linejoin="round" aria-hidden="true">
                <rect x="3.5" y="7" width="9" height="6.5" rx="1.5"></rect>
                <path d="M5.5 7V5a2.5 2.5 0 015 0v2"></path>
            </svg>
        }
    }

    pub async fn bin(cx: &Cx) -> Result {
        view! {
            cx =>
            <svg class="glyph" width="14" height="14" viewBox="0 0 16 16" fill="none"
                stroke="currentColor" stroke-width="1.5" stroke-linecap="round"
                stroke-linejoin="round" aria-hidden="true">
                <path d="M3 4.5h10M6.5 4.5V3h3v1.5M4.5 4.5l0.7 8.5a1 1 0 001 0.9h3.6a1 1 0 001-0.9l0.7-8.5"></path>
            </svg>
        }
    }
}

/// `Escape` for everything the task modal renders, topmost first: viewer →
/// delete confirm (`confirm=delete` in URL) → open edit popovers → modal
/// itself — one resolver at priority 90 on `window.__izlekEsc` (table on
/// `layout.rs`'s `escape_manager_script`); an open datepick popover returns
/// false, leaving it to `board.rs`'s priority-100 resolver. Closing never
/// navigates: `layout.rs`'s `__izlekCloseViewer`/`__izlekCloseModal` drop
/// the overlay's DOM and rewrite the URL, the board underneath is already
/// rendered. Focused native media controls swallow `Escape` before the page
/// ever sees the key, so focus landing on the viewer's audio/video is moved
/// straight back to the panel.
async fn escape_closes(cx: &Cx) -> Result {
    use topcoat::view::Unescaped;
    const JS: &str = "\
        (function () { \
        if (window.__izlekEscModal) { return; } \
        window.__izlekEscModal = true; \
        window.__izlekEsc.register(90, function () { \
            if (document.querySelector('.datepick-pop .edit-toggle:checked')) { return false; } \
            if (document.querySelector('.viewer-scrim')) { \
                window.__izlekCloseViewer(); \
                return true; \
            } \
            var confirm = document.querySelector('details.confirm-details[open]'); \
            if (confirm && window.location.search.indexOf('confirm=delete') !== -1) { \
                window.__izlekCloseModal(); \
                return true; \
            } \
            var toggled = false; \
            document.querySelectorAll('.edit-toggle:checked').forEach(function (toggle) { \
                if (toggle.closest('.datepick-pop')) { return; } \
                var edit = toggle.closest('.edit'); \
                if (edit) { toggle.checked = false; toggled = true; } \
            }); \
            if (toggled || confirm) { \
                if (confirm) { confirm.removeAttribute('open'); } \
                return true; \
            } \
            if (document.querySelector('.modal-scrim')) { \
                window.__izlekCloseModal(); \
                return true; \
            } \
            return false; \
        }); \
        document.addEventListener('focusin', function (e) { \
            var media = e.target; \
            if (media.tagName !== 'AUDIO' && media.tagName !== 'VIDEO') { return; } \
            var panel = media.closest('.viewer'); \
            if (panel) { panel.focus(); } \
        }, true); \
        document.addEventListener('click', function (e) { var confirm = document.querySelector('details.confirm-details[open]'); if (!confirm) { return; } var panel = confirm.querySelector('.confirm'); if (panel && !panel.contains(e.target) && !e.target.closest('summary')) { if (window.location.search.indexOf('confirm=delete') !== -1) { window.__izlekCloseModal(); } else { confirm.removeAttribute('open'); } } }, true); \
        document.addEventListener('click', function (e) { \
            document.querySelectorAll('.edit-toggle:checked').forEach(function (toggle) { \
                if (toggle.closest('.datepick-pop')) { return; } \
                var edit = toggle.closest('.edit'); \
                if (edit && !edit.contains(e.target)) { toggle.checked = false; } \
            }); \
        }, true); \
        })();";
    view! { cx => <script>(Unescaped::new_unchecked(JS))</script> }
}

async fn refused(cx: &Cx, call: &str, lang: Lang) -> Result {
    match refusal_of(cx, call) {
        Some(refusal) => view! { cx => <p class="modal-problem">(refusal.message_in(lang))</p> },
        None => view! { cx => },
    }
}

async fn title_control(cx: &Cx, task: &TaskDetail, may_write: bool, lang: Lang) -> Result {
    if !may_write {
        return view! { cx => <h2 class="detail-title">(task.title.clone())</h2> };
    }
    let toggle = format!("rename-{}", task.id);
    let rename_aria = t(lang, Key::RenameThisTask);
    view! {
        cx =>
        <div class="edit">
            <input class="edit-toggle" type="checkbox" id=(toggle.clone()) aria-label=(rename_aria)>
            <h2 class="detail-title edit-view">
                <label class="edit-hit" for=(toggle.clone())>(task.title.clone())</label>
            </h2>
            <form class="edit-form title-form" method="post" action="/api/save_task">
                <input type="hidden" name="task_id" value=(task.id.clone())>
                <input class="title-input" type="text" name="title" value=(task.title.clone()) autocomplete="off" required="">
                <button class="edit-save" type="submit">(t(lang, Key::Save))</button>
                <label class="edit-cancel" for=(toggle)>(t(lang, Key::Cancel))</label>
            </form>
            (refused(cx, "save_task", lang).await?)
        </div>
    }
}

async fn description_control(cx: &Cx, task: &TaskDetail, may_write: bool, lang: Lang) -> Result {
    let empty = task.description.trim().is_empty();
    let no_description = t(lang, Key::NoDescription).to_string();
    let prose = if empty {
        no_description.clone()
    } else {
        task.description.clone()
    };

    if !may_write {
        return view! { cx => <p class=(class!("detail-prose", "detail-prose-empty" if empty))>(prose)</p> };
    }
    let toggle = format!("describe-{}", task.id);
    let edit_aria = t(lang, Key::EditTheDescription);
    view! {
        cx =>
        <div class="edit">
            <input class="edit-toggle" type="checkbox" id=(toggle.clone()) aria-label=(edit_aria)>
            <label class=(class!("detail-prose", "edit-view", "edit-hit", "detail-prose-empty" if empty)) for=(toggle.clone()) data-empty=(no_description)>
                (prose)
            </label>
            <form class="edit-form describe-form" method="post" action="/api/save_task">
                <input type="hidden" name="task_id" value=(task.id.clone())>
                <textarea class="detail-textarea" name="description" rows="5">(task.description.clone())</textarea>
                <div class="edit-row">
                    <button class="edit-save" type="submit">(t(lang, Key::Save))</button>
                    <label class="edit-cancel" for=(toggle)>(t(lang, Key::Cancel))</label>
                </div>
            </form>
            (refused(cx, "save_task", lang).await?)
        </div>
    }
}

/// The moment field: the deadline's calendar popover, with one optional
/// time box under the grid. It used to carry Save and Cancel under the
/// grid, which put the commit two gestures away from the choice — and
/// picking a day closes the popover, so the Save it was waiting for was no
/// longer on screen. Every date chosen this way was silently discarded.
/// The grid now autosubmits, the way the log filters already did, and
/// closing is what Escape and a click outside are for. The time rides the
/// same commit: a posted form carries whatever is typed, so `16:20` plus a
/// pressed day is one exact moment, and Clear empties both. A time the day
/// can already carry commits by itself — the two boxes submit when they
/// agree (both set or both emptied) and the input holds a day, because a
/// lone box is a half-written value, not a moment, and a time with no day
/// anywhere is not one either. On a dayless task the picks wait for the
/// pressed day and ride its commit.
///
/// The box speaks the moment when there is one, the day when there is only
/// a day — the clock's day is always the deadline's, and the label reads in
/// the viewer's own zone.
async fn deadline_control(
    cx: &Cx,
    task: &TaskDetail,
    today: Date,
    zone: UtcOffset,
    may_write: bool,
    lang: Lang,
) -> Result {


    let overdue = task.is_overdue(today);
    let local = task.clock_at.map(|at| at.to_offset(zone));
    let label = match (&local, task.deadline_parts(today)) {
        (Some(at), Some(parts)) if parts.state == DeadlineState::Overdue => {
            format!(
                "{} {:02}:{:02} · {}",
                parts.date,
                at.hour(),
                at.minute(),
                t(lang, Key::Overdue)
            )
        }
        (Some(at), Some(parts)) => format!("{} {:02}:{:02}", parts.date, at.hour(), at.minute()),
        (Some(at), None) => format!(
            "{} {:02}:{:02}",
            izlek_core::board::day_label(at.date()),
            at.hour(),
            at.minute()
        ),
        (None, Some(parts)) if parts.state == DeadlineState::Overdue => {
            format!("{} · {}", parts.date, t(lang, Key::Overdue))
        }
        (None, Some(parts)) => parts.date,
        (None, None) => t(lang, Key::NoDeadline).to_string(),
    };
    if !may_write {
        return view! {
            cx =>
            <span class=(class!("field-box", "detail-overdue" if overdue))>
                (glyph::calendar(cx).await?)
                <span class="field-text">(label)</span>
            </span>
        };
    }
    let toggle = format!("deadline-{}", task.id);
    let input_value = task.deadline_input();
    let hm = local.map(|at| (at.hour(), at.minute()));
    let change_aria = t(lang, Key::ChangeTheDeadline);
    let no_deadline = t(lang, Key::NoDeadline);
    view! {
        cx =>
        <div class="edit edit-pop datepick-pop">
            <input class="edit-toggle" type="checkbox" id=(toggle.clone()) aria-label=(change_aria)>
            <label class=(class!("field-box", "edit-view", "edit-hit", "detail-overdue" if overdue)) for=(toggle.clone())>
                (glyph::calendar(cx).await?)
                <span class="field-text datepick-label" data-empty=(no_deadline)>(label)</span>
                (glyph::chevron(cx).await?)
            </label>
            <div class="edit-form pop-panel datepick-panel">
                <form class="pop-form" method="post" action="/api/save_task">
                    <input type="hidden" name="task_id" value=(task.id.clone())>
                    (datepicker_grid(cx, "deadline", &input_value, true, lang).await?)
                    <div class="datepick-time">
                        <select class="field-input" name="clock_hour" aria-label=(t(lang, Key::ClockHour)) data-search="">
                            <option value="">("--")</option>
                            for hour in 0u8..24 {
                                <option value=(format!("{hour:02}")) selected=(hm.map(|(h, _)| h) == Some(hour))>(format!("{hour:02}"))</option>
                            }
                        </select>
                        <span class="datepick-colon">(":")</span>
                        <select class="field-input" name="clock_minute" aria-label=(t(lang, Key::ClockMinute)) data-search="">
                            <option value="">("--")</option>
                            for minute in 0u8..60 {
                                <option value=(format!("{minute:02}")) selected=(hm.map(|(_, m)| m) == Some(minute))>(format!("{minute:02}"))</option>
                            }
                        </select>
                    </div>
                </form>
                (refused(cx, "save_task", lang).await?)
            </div>
        </div>
    }
}

/// The calendar grid's static shell inside a `.datepick-panel`: a hidden
/// `<input>` carrying the real `yyyy-mm-dd` value, month nav, and the day
/// grid — the grid cells and month title are blank until `datepicker_script`
/// fills them in on open (see that function's doc comment for why the fill
/// happens in JS rather than here).
pub(crate) async fn datepicker_grid(
    cx: &Cx,
    name: &str,
    value: &str,
    autosubmit: bool,
    lang: Lang,
) -> Result {
    view! {
        cx =>
        <input class="datepick-input" type="hidden" name=(name.to_string()) value=(value.to_string()) data-autosubmit=(autosubmit)>
        <div class="datepick-head">
            <button class="datepick-nav datepick-prev" type="button" aria-label=(t(lang, Key::PreviousMonth))>(glyph::chevron(cx).await?)</button>
            <span class="datepick-title"></span>
            <button class="datepick-nav datepick-next" type="button" aria-label=(t(lang, Key::NextMonth))>(glyph::chevron(cx).await?)</button>
        </div>
        <div class="datepick-weekdays"></div>
        <div class="datepick-grid"></div>
        <div class="datepick-foot">
            <button class="datepick-action datepick-clear" type="button">(t(lang, Key::Clear))</button>
            <button class="datepick-action datepick-today" type="button">(t(lang, Key::Today))</button>
        </div>
    }
}

/// The house calendar's month grid, wholly in JS: this is a plain document
/// reload per page (no framework runtime to hand a computed grid to), so the
/// grid is built client-side from `<input class=datepick-input>`'s value
/// and the browser's own "today" — mirroring the old wasm build's
/// `js_sys::Date`-driven `DeadlineControl`, minus the wasm. Wires month
/// nav, day pick, Clear/Today, and closes a panel on outside click —
/// registered in the capture phase so a stray click never reaches
/// `escape_closes`'s confirm-closing listener while a panel is open.
/// `Escape` closes the panel through the datepick resolver `board.rs`'s
/// `card_menu_script` registers (priority 100 — see the table on
/// `layout.rs`'s `escape_manager_script`).
///
/// The popover is the surface the live refresh used to reset. A picked day
/// lives in the hidden input's `value` — and on a hidden input the property
/// *is* the attribute, so the morph's attribute sync read it as server-stale
/// bytes and wrote the old date back over the pick; the label it reverted,
/// the client-built grid it emptied, the month nav's `data-year`/`data-month`
/// it swept. So the pick declares itself the client's the moment it happens
/// (`__izlekOwn(input, [], ['value'])`, and the panel's month attrs when
/// `render` first sets them), and an `izlek:wire` repair pass re-derives the
/// rest from that value: the label of a popover holding an unsaved pick, and
/// the grid of one standing open. The pass runs on every wire — a repair,
/// not an install; closed popovers with no unsaved pick are left to the
/// server's own voice.
pub(crate) async fn datepicker_script(cx: &Cx, lang: Lang) -> Result {
    use crate::i18n::datepicker_js_literals;
    use topcoat::view::Unescaped;
    let (months, weekdays) = datepicker_js_literals(lang);
    let js = format!(
        "(function() {{\
            if (window.__izlekDatepick) {{ return; }}\
            window.__izlekDatepick = true;\
            var MONTHS = {months};\
            var WEEKDAYS = {weekdays};\
            function pad(n) {{ return String(n).padStart(2, '0'); }}\
            function parseYmd(v) {{ if (!v) return null; var p = v.split('-'); if (p.length !== 3) return null; return {{ y: +p[0], m: +p[1], d: +p[2] }}; }}\
            function todayYmd() {{ var t = new Date(); return {{ y: t.getFullYear(), m: t.getMonth() + 1, d: t.getDate() }}; }}\
            function speak(label, ymd) {{ label.textContent = ymd ? MONTHS[ymd.m - 1].slice(0, 3) + ' ' + pad(ymd.d) : (label.dataset.empty || ''); }}\
            function render(panel) {{\
                var input = panel.querySelector('.datepick-input');\
                if (!input) {{ return; }}\
                var sel = parseYmd(input.value);\
                var t = todayYmd();\
                if (!panel.dataset.year) {{ var base = sel || t; panel.dataset.year = base.y; panel.dataset.month = base.m; window.__izlekOwn(panel, [], ['data-year', 'data-month']); }}\
                var y = +panel.dataset.year, m = +panel.dataset.month;\
                panel.querySelector('.datepick-title').textContent = MONTHS[m - 1] + ' ' + y;\
                panel.querySelector('.datepick-weekdays').innerHTML = WEEKDAYS.map(function(w) {{ return '<span>' + w + '</span>'; }}).join('');\
                var lead = (new Date(y, m - 1, 1).getDay() + 6) % 7;\
                var days = new Date(y, m, 0).getDate();\
                var cells = [];\
                for (var i = 0; i < lead; i++) {{ cells.push('<span class=\\\"datepick-cell\\\"></span>'); }}\
                for (var d = 1; d <= days; d++) {{\
                    var isToday = t.y === y && t.m === m && t.d === d;\
                    var isSel = sel && sel.y === y && sel.m === m && sel.d === d;\
                    var cls = 'datepick-cell datepick-day' + (isToday ? ' datepick-today' : '') + (isSel ? ' datepick-selected' : '');\
                    cells.push('<button type=\\\"button\\\" class=\\\"' + cls + '\\\" data-day=\\\"' + d + '\\\">' + d + '</button>');\
                }}\
                panel.querySelector('.datepick-grid').innerHTML = cells.join('');\
            }}\
            function pick(panel, ymd) {{\
                var input = panel.querySelector('.datepick-input');\
                if (!input) {{ return; }}\
                input.value = ymd ? (ymd.y + '-' + pad(ymd.m) + '-' + pad(ymd.d)) : '';\
                window.__izlekOwn(input, [], ['value']);\
                if (!ymd) {{ ['clock_hour', 'clock_minute'].forEach(function (name) {{ var box = panel.querySelector('[name=' + name + ']'); if (!box) {{ return; }} box.value = ''; var trig = box.__ddTrigger; if (trig) {{ trig.textContent = box.options[0].textContent; trig.setAttribute('aria-expanded', 'false'); }} var drop = box.__ddPanel; if (drop) {{ drop.classList.remove('dd-open'); drop.querySelectorAll('.dd-option-selected').forEach(function (r) {{ r.classList.remove('dd-option-selected'); r.setAttribute('aria-selected', 'false'); }}); }} }}); }} \
                var pop = panel.closest('.datepick-pop');\
                var label = pop.querySelector('.datepick-label');\
                if (label) {{ speak(label, ymd); }}\
                var toggle = pop.querySelector('.edit-toggle');\
                if (toggle) {{ toggle.checked = false; }}\
                if (input.hasAttribute('data-autosubmit') && input.form) {{ input.form.requestSubmit(); }}\
            }}\
            document.addEventListener('change', function(e) {{\
                var toggle = e.target.closest('.datepick-pop > .edit-toggle');\
                if (!toggle || !toggle.checked) {{ return; }}\
                var panel = toggle.closest('.datepick-pop').querySelector('.datepick-panel');\
                if (panel) {{ render(panel); }}\
            }});\
            document.addEventListener('change', function(e) {{\
                var box = e.target;\
                if (!box.matches || !box.matches('.datepick-time select')) {{ return; }}\
                var panel = box.closest('.datepick-panel');\
                var input = panel && panel.querySelector('.datepick-input');\
                if (!input || !input.hasAttribute('data-autosubmit') || !input.value) {{ return; }}\
                var hour = panel.querySelector('[name=clock_hour]');\
                var minute = panel.querySelector('[name=clock_minute]');\
                if (!hour || !minute) {{ return; }}\
                if (!!hour.value !== !!minute.value) {{ return; }}\
                if (input.form) {{ input.form.requestSubmit(); }}\
            }});\
            document.addEventListener('izlek:wire', function () {{\
                document.querySelectorAll('.datepick-pop').forEach(function (pop) {{\
                    var panel = pop.querySelector('.datepick-panel');\
                    if (!panel) {{ return; }}\
                    var toggle = pop.querySelector('.edit-toggle');\
                    if (toggle && toggle.checked) {{ render(panel); }}\
                    var input = panel.querySelector('.datepick-input');\
                    if (!input || !input.__izlekMine || input.__izlekMine.a.indexOf('value') === -1) {{ return; }}\
                    var label = pop.querySelector('.datepick-label');\
                    if (label) {{ speak(label, parseYmd(input.value)); }}\
                }});\
            }});\
            document.addEventListener('click', function(e) {{\
                var prev = e.target.closest('.datepick-prev');\
                var next = e.target.closest('.datepick-next');\
                var day = e.target.closest('.datepick-day');\
                var clear = e.target.closest('.datepick-clear');\
                var today = e.target.closest('.datepick-today');\
                if (prev || next) {{\
                    var panel = e.target.closest('.datepick-panel');\
                    var y = +panel.dataset.year, m = +panel.dataset.month;\
                    if (prev) {{ m -= 1; if (m < 1) {{ m = 12; y -= 1; }} }} else {{ m += 1; if (m > 12) {{ m = 1; y += 1; }} }}\
                    panel.dataset.year = y; panel.dataset.month = m;\
                    render(panel);\
                    return;\
                }}\
                if (day) {{\
                    var panel = e.target.closest('.datepick-panel');\
                    pick(panel, {{ y: +panel.dataset.year, m: +panel.dataset.month, d: +day.dataset.day }});\
                    return;\
                }}\
                if (clear) {{ pick(e.target.closest('.datepick-panel'), null); return; }}\
                if (today) {{\
                    var panel = e.target.closest('.datepick-panel');\
                    var t = todayYmd();\
                    panel.dataset.year = t.y; panel.dataset.month = t.m;\
                    pick(panel, t);\
                    return;\
                }}\
                var openPop = document.querySelector('.datepick-pop > .edit-toggle:checked');\
                if (openPop && !openPop.closest('.datepick-pop').contains(e.target) && !e.target.closest('.dd-panel')) {{ openPop.checked = false; }} \
            }}, true);\
        }})();"
    );
    view! { cx => <script>(Unescaped::new_unchecked(js))</script> }
}

async fn assignee_chip(
    cx: &Cx,
    task_id: &str,
    person: &Person,
    may_write: bool,
    lang: Lang,
) -> Result {
    let remove_title = crate::i18n::take_off_this_task(lang, &person.display_name);
    view! {
        cx =>
        <span class="assignee-chip">
            <a class="person-link" href=(format!("/people/{}", person.id))>
                (crate::layout::avatar(cx, person, "avatar-sm").await?)
                <span class="assignee-name">(person.display_name.clone())</span>
            </a>
            if may_write {
                <form class="assignee-drop" method="post" action="/api/unassign">
                    <input type="hidden" name="task_id" value=(task_id.to_string())>
                    <input type="hidden" name="user_id" value=(person.id.clone())>
                    <button class="assignee-remove" type="submit" title=(remove_title)>(glyph::cross(cx).await?)</button>
                </form>
            }
        </span>
    }
}

async fn assignee_picker(cx: &Cx, task_id: &str, people: &[Person], lang: Lang) -> Result {
    if people.is_empty() {
        return view! { cx => };
    }
    let toggle = format!("assign-{task_id}");
    let put_aria = t(lang, Key::PutSomeoneOnThisTask);
    view! {
        cx =>
        <div class="edit edit-pop assignee-pop">
            <input class="edit-toggle" type="checkbox" id=(toggle.clone()) aria-label=(put_aria)>
            <label class="assignee-add edit-view edit-hit" for=(toggle.clone())>(glyph::plus(cx).await?)</label>
            <div class="edit-form pop-panel">
                <div class="pop-list pop-list-scroll">
                    for person in people {
                        <form class="pop-row-form" method="post" action="/api/assign">
                            <input type="hidden" name="task_id" value=(task_id.to_string())>
                            <input type="hidden" name="user_id" value=(person.id.clone())>
                            <button class="pop-row" type="submit">
                                (crate::layout::avatar(cx, person, "avatar-sm").await?)
                                <span class="pop-row-name">(person.display_name.clone())</span>
                            </button>
                        </form>
                    }
                </div>
                (refused(cx, "assign", lang).await?)
            </div>
        </div>
    }
}

async fn link_picker(cx: &Cx, task_id: &str, linkable: &[LinkTarget], lang: Lang) -> Result {
    if linkable.is_empty() {
        return view! { cx => };
    }
    let toggle = format!("link-{task_id}");
    let link_aria = t(lang, Key::LinkAnotherTask);
    view! {
        cx =>
        <div class="edit edit-pop link-pop">
            <input class="edit-toggle" type="checkbox" id=(toggle.clone()) aria-label=(link_aria)>
            <label class="dep-chip edit-view edit-hit" for=(toggle.clone())>
                (glyph::plus(cx).await?)
                <span class="dep-chip-text">(t(lang, Key::LinkATask))</span>
            </label>
            <div class="edit-form pop-panel pop-panel-wide">
                <form class="pop-form" method="post" action="/api/link_tasks">
                    <input type="hidden" name="task_id" value=(task_id.to_string())>
                    <div class="pop-list pop-list-scroll">
                        for target in linkable {
                            <label class="pick-row">
                                <input type="radio" name="other_id" value=(target.id.clone()) required="">
                                <span class="dep-key">(target.task_key.clone())</span>
                                <span class="pick-title">(target.title.clone())</span>
                            </label>
                        }
                    </div>
                    <fieldset class="pick-direction">
                        <legend class="detail-label">(t(lang, Key::Direction))</legend>
                        <label class="pick-row">
                            <input type="radio" name="direction" value="blocked_by" checked="">
                            <span class="pick-title">(t(lang, Key::BlocksThisTask))</span>
                        </label>
                        <label class="pick-row">
                            <input type="radio" name="direction" value="blocks">
                            <span class="pick-title">(t(lang, Key::WaitsOnThisTask))</span>
                        </label>
                    </fieldset>
                    <div class="edit-row">
                        <button class="edit-save" type="submit">(t(lang, Key::Link))</button>
                        <label class="edit-cancel" for=(toggle)>(t(lang, Key::Cancel))</label>
                    </div>
                </form>
                (refused(cx, "link_tasks", lang).await?)
            </div>
        </div>
    }
}

/// One part on its parent's page: its key, its title, who holds it, and the
/// button that lets it out. The status is the column's name, read off the
/// board's columns the page already has.
async fn subtask_row(
    cx: &Cx,
    part: &izlek_core::detail::SubtaskLine,
    columns: &[izlek_core::Column],
    may_write: bool,
    lang: Lang,
) -> Result {
    let done = part.is_done();
    let status = columns
        .iter()
        .find(|column| column.id == part.column_id)
        .map(|column| column.name.clone())
        .unwrap_or_default();
    let href = format!("/?task={}", part.id);
    let release_title = t(lang, Key::ReleaseThisPart);
    let mut faces = Vec::new();
    for person in &part.assignees {
        faces.push(crate::layout::avatar(cx, person, "").await?);
    }
    view! {
        cx =>
        <div class=(class!("subtask-row", "subtask-row-done" if done))>
            // One link, not two: the key and the title are the same target,
            // so the whole of the row's text is the way in.
            <a class="subtask-open" href=(href)>
                <span class="subtask-mark">
                    if done { (glyph::tick(cx).await?) } else { (glyph::ring(cx).await?) }
                </span>
                <span class="subtask-key">(part.task_key.clone())</span>
                <span class="subtask-title">(part.title.clone())</span>
            </a>
            <div class="spacer"></div>
            <span class="subtask-status">(status)</span>
            <div class="avatars">
                for face in faces { (face) }
            </div>
            if may_write {
                <form class="dep-unlink-form" method="post" action="/api/set_parent">
                    <input type="hidden" name="task_id" value=(part.id.clone())>
                    <input type="hidden" name="parent_id" value="">
                    <button class="dep-unlink" type="submit" title=(release_title)>(glyph::cross(cx).await?)</button>
                </form>
            }
        </div>
    }
}

/// Takes an existing task in as a part. The same shape as `link_picker`, and
/// absent for the same reason: with nothing to offer it shows nothing.
async fn adopt_picker(cx: &Cx, task_id: &str, adoptable: &[LinkTarget], lang: Lang) -> Result {
    if adoptable.is_empty() {
        return view! { cx => };
    }
    let toggle = format!("adopt-{task_id}");
    let aria = t(lang, Key::MakeAPart);
    view! {
        cx =>
        <div class="edit edit-pop link-pop">
            <input class="edit-toggle" type="checkbox" id=(toggle.clone()) aria-label=(aria)>
            <label class="dep-chip edit-view edit-hit" for=(toggle.clone())>
                (glyph::plus(cx).await?)
                <span class="dep-chip-text">(t(lang, Key::MakeAPart))</span>
            </label>
            <div class="edit-form pop-panel pop-panel-wide">
                <form class="pop-form" method="post" action="/api/set_parent">
                    <input type="hidden" name="parent_id" value=(task_id.to_string())>
                    <div class="pop-list pop-list-scroll">
                        for target in adoptable {
                            <label class="pick-row">
                                <input type="radio" name="task_id" value=(target.id.clone()) required="">
                                <span class="dep-key">(target.task_key.clone())</span>
                                <span class="pick-title">(target.title.clone())</span>
                            </label>
                        }
                    </div>
                    <div class="edit-row">
                        <button class="edit-save" type="submit">(t(lang, Key::ExistingTask))</button>
                        <label class="edit-cancel" for=(toggle)>(t(lang, Key::Cancel))</label>
                    </div>
                </form>
            </div>
        </div>
    }
}

async fn dep_row(
    cx: &Cx,
    task_id: &str,
    edge: &DependencyEdge,
    direction: Direction,
    may_write: bool,
    lang: Lang,
) -> Result {
    let cleared = edge.is_cleared();
    let note = match direction {
        Direction::BlockedBy => edge.blocked_by_label(),
        Direction::Blocks => edge.blocks_label(),
    };
    let waiting = matches!(direction, Direction::BlockedBy) && !cleared;
    let wire = match direction {
        Direction::BlockedBy => "blocked_by",
        Direction::Blocks => "blocks",
    };
    let tag = match direction {
        Direction::BlockedBy => t(lang, Key::BlockedBy).to_uppercase(),
        Direction::Blocks => t(lang, Key::Blocks).to_uppercase(),
    };
    let remove_title = t(lang, Key::RemoveThisLink);
    view! {
        cx =>
        <div class=(class!("dep-row", "dep-row-waiting" if waiting))>
            <span class="dep-tag">(tag)</span>
            if cleared { (glyph::tick(cx).await?) } else { (glyph::lock(cx).await?) }
            <span class="dep-key">(edge.task_key.clone())</span>
            <span class="dep-title">(edge.title.clone())</span>
            <div class="spacer"></div>
            <span class="dep-note">(note)</span>
            if may_write {
                <form class="dep-unlink-form" method="post" action="/api/unlink_tasks">
                    <input type="hidden" name="task_id" value=(task_id.to_string())>
                    <input type="hidden" name="other_id" value=(edge.task_id.clone())>
                    <input type="hidden" name="direction" value=(wire)>
                    <button class="dep-unlink" type="submit" title=(remove_title)>(glyph::cross(cx).await?)</button>
                </form>
            }
        </div>
    }
}

/// The version stamp a `/files/{id}` URL carries: the row's `uploaded_at`.
/// Uploads are insert-only — nothing in the store rewrites an attachment —
/// so the stamp is the one moment the bytes have ever changed, and a URL
/// that carries it can be cached as if it were the bytes themselves.
fn attachment_stamp(attachment: &izlek_core::store::Attachment) -> String {
    (attachment.uploaded_at.unix_timestamp_nanos() / 1_000).to_string()
}

async fn file_chip(
    cx: &Cx,
    task_id: &str,
    file: &izlek_core::detail::FileLine,
    me: &Me,
    may_write: bool,
    lang: Lang,
) -> Result {
    let _ = may_write;
    let may_drop = me.id == file.uploaded_by || me.role.can_administer();
    let on_comment = file.comment_id.is_some();
    let remove_title = t(lang, Key::RemoveThisFile);
    let href = if crate::files::viewer_kind(&file.mime_type).is_some() {
        format!(
            "/?task={task_id}&tab={}&file={}",
            Tab::Files.slug(),
            file.id
        )
    } else {
        // The stamp rides on the row, which the chip's `FileLine` does not
        // carry: one lookup per plain-download chip buys the same versioned
        // URL the viewer streams from.
        match accounts(cx).store().attachment(&file.id).await {
            Ok(Some(row)) => format!("/files/{}?v={}", file.id, attachment_stamp(&row)),
            _ => format!("/files/{}", file.id),
        }
    };
    view! {
        cx =>
        <span class="file-chip">
            <a class="file-chip-name" href=(href)>(file.name.clone())</a>
            <span class="file-chip-size">(file.size_label())</span>
            if on_comment {
                <span class="file-chip-note">(t(lang, Key::OnAComment))</span>
            }
            if may_drop {
                <form class="file-chip-drop-form" method="post" action="/api/delete_file">
                    <input type="hidden" name="file_id" value=(file.id.clone())>
                    <button class="file-chip-drop" type="submit" title=(remove_title)>(glyph::cross(cx).await?)</button>
                </form>
            }
        </span>
    }
}

async fn comment_row(cx: &Cx, comment: &Comment, zone: UtcOffset) -> Result {
    view! {
        cx =>
        <div class="comment">
            <a class="person-link" href=(format!("/people/{}", comment.author.id))>
                (crate::layout::avatar(cx, &comment.author, "avatar-lg").await?)
            </a>
            <div class="comment-said">
                <div class="comment-head">
                    <a class="comment-who person-link" href=(format!("/people/{}", comment.author.id))>(comment.author.display_name.clone())</a>
                    <span class="comment-when">(moment_label_in(comment.at, zone))</span>
                </div>
                <div class="comment-body">(comment.body.clone())</div>
            </div>
        </div>
    }
}

/// The task modal's markup: title, description, assignees, deadline,
/// dependencies, files, comments, activity and delete, exactly as the
/// Which region of the task detail panel is showing. Only one is drawn at a
/// time; a `tab=` name it does not recognize falls back to `Task`, same as
/// `logs.rs`/`settings.rs`'s own rail sections.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Task,
    Subtasks,
    Files,
    Comments,
    Activity,
    Mail,
}

impl Tab {
    pub fn from_query(tab: Option<&str>) -> Tab {
        match tab {
            Some("subtasks") => Tab::Subtasks,
            Some("files") => Tab::Files,
            Some("comments") => Tab::Comments,
            Some("activity") => Tab::Activity,
            Some("mail") => Tab::Mail,
            _ => Tab::Task,
        }
    }

    fn slug(self) -> &'static str {
        match self {
            Tab::Task => "task",
            Tab::Subtasks => "subtasks",
            Tab::Files => "files",
            Tab::Comments => "comments",
            Tab::Activity => "activity",
            Tab::Mail => "mail",
        }
    }
}

/// The class a tab link wears: lit on the tab it points to when that is the
/// one showing, plain otherwise — same idiom as `logs.rs`/`settings.rs`'s
/// `rail_class`.
fn tab_class(current: Tab, target: Tab) -> &'static str {
    if current == target {
        "detail-tab detail-tab-on"
    } else {
        "detail-tab"
    }
}

/// artboard draws them. Wiring `?task=<id>` on the board page is a later
/// integration slice — this only renders the fragment.
pub async fn task_modal(cx: &Cx, task_id: &str, confirm_delete: bool, tab: Tab) -> Result {
    let snapshot = match load_snapshot(cx, task_id).await? {
        Ok(snapshot) => snapshot,
        Err(refusal) => {
            // A refused snapshot can still have a signed-in user behind it —
            // a gone task id is the common case — so the language is read
            // the same way `board.rs`'s query-refusal branch does, English
            // only when there truly is nobody signed in to read one off of.
            let lang = require_user(cx)
                .await
                .map(|user| Lang::from_code(&user.language))
                .unwrap_or(Lang::En);
            return view! {
                cx =>
                <div class="modal-scrim">
                    <div class="modal modal-task"><p class="modal-note">(refusal.message_in(lang))</p></div>
                </div>
                (escape_closes(cx).await?)
            };
        }
    };
    let DetailSnapshot {
        detail,
        me,
        today,
        zone,
        linkable,
        adoptable,
        may_write,
        may_comment,
        may_delete,
        allowed_file_types,
        attachment_limit_mb: _,
        notifications,
        tags,
        may_administer: _,
    } = snapshot;
    let lang = Lang::from_code(&me.language);

    let unassigned: Vec<Person> = detail.unassigned().cloned().collect();
    let done_parts = detail.subtasks.iter().filter(|part| part.is_done()).count();
    // A subtask is never offered the Subtasks tab, so an address bar that
    // still names it lands on the task itself rather than on an empty panel
    // with no tab lit.
    let tab = if tab == Tab::Subtasks && detail.parent.is_some() {
        Tab::Task
    } else {
        tab
    };
    let has_deps = !detail.blocked_by.is_empty() || !detail.blocks.is_empty();
    let accept = (!allowed_file_types.is_empty())
        .then(|| {
            allowed_file_types
                .iter()
                .map(|kind| format!(".{kind}"))
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default();

    // The delete confirmation is computed eagerly rather than fetched on
    // demand: there is no script here to hold the intermediate "did they
    // click delete yet" state, so the cost is already known by the time the
    // disclosure opens.
    let cost = if may_delete {
        accounts(cx).store().deletion_cost(&detail.id).await?
    } else {
        None
    };

    view! {
        cx =>
        <div class="modal-scrim">
            <div class="modal modal-task" tabindex="-1">
                // Key, title, state and the tabs ride together as one masthead:
                // pinned, so the task still names itself at the 200th comment.
                <div class="detail-mast">
                    <header class="detail-head">
                        <div class="detail-headline">
                            // One line, read left to right: the whole, then
                            // this. Two keys stacked in the same mono said
                            // nothing about which was which — order does.
                            <div class="detail-crumbs">
                                if let Some(parent) = detail.parent.clone() {
                                    <a class="detail-crumb detail-crumb-up" href=(format!("/?task={}", parent.id))>
                                        <span class="detail-crumb-key">(parent.task_key.clone())</span>
                                        <span class="detail-crumb-title">(parent.title.clone())</span>
                                    </a>
                                    <span class="detail-crumb-sep" aria-hidden="true">"\u{203a}"</span>
                                }
                                <span class="detail-key">(detail.task_key.clone())</span>
                            </div>
                            (title_control(cx, &detail, may_write, lang).await?)
                        </div>
                        <span class="detail-state">
                            if may_write {
                                <form class="status-form" method="post" action="/api/move_card">
                                    <input type="hidden" name="task_id" value=(detail.id.clone())>
                                    <input type="hidden" name="from_column_id" value=(detail.column.id.clone())>
                                    <span class=(class!("status-dot", "status-dot-done" if detail.column.is_done))></span>
                                    <select class="status-select" name="to_column_id" data-autosubmit="">
                                        for column in &detail.columns {
                                            <option value=(column.id.clone()) selected=(column.id == detail.column.id)>(column.name.clone())</option>
                                        }
                                    </select>
                                    (glyph::chevron(cx).await?)
                                </form>
                            } else {
                                <span class="field-box">
                                    <span class="status-dot"></span>
                                    <span class="field-text">(detail.column.name.clone())</span>
                                </span>
                            }
                        </span>
                        <div class="spacer"></div>
                        <span class="detail-esc">(t(lang, Key::Esc))</span>
                        <a class="detail-close" href="/" aria-label=(t(lang, Key::CloseThisTask))>(glyph::cross(cx).await?)</a>
                        <a class="quiet detail-board" href="/">(format!("<- {}", t(lang, Key::NavBoard)))</a>
                    </header>

                    <nav class="detail-tabs">
                        <a class=(tab_class(tab, Tab::Task)) href=(format!("/?task={}&tab={}", detail.id, Tab::Task.slug()))>(t(lang, Key::TabTask))</a>
                        // A subtask can never have parts of its own, so it is
                        // never offered the tab: one level deep is a rule of
                        // the model, not something to discover from an empty
                        // list.
                        if detail.parent.is_none() {
                            <a class=(tab_class(tab, Tab::Subtasks)) href=(format!("/?task={}&tab={}", detail.id, Tab::Subtasks.slug()))>
                                (t(lang, Key::Subtasks))
                                if !detail.subtasks.is_empty() {
                                    <span class="detail-tab-count">(format!("{}/{}", done_parts, detail.subtasks.len()))</span>
                                }
                            </a>
                        }
                        <a class=(tab_class(tab, Tab::Files)) href=(format!("/?task={}&tab={}", detail.id, Tab::Files.slug()))>
                            (t(lang, Key::Files))
                            if !detail.files.is_empty() { <span class="detail-tab-count">(detail.files.len())</span> }
                        </a>
                        <a class=(tab_class(tab, Tab::Comments)) href=(format!("/?task={}&tab={}", detail.id, Tab::Comments.slug()))>
                            (t(lang, Key::Comments))
                            if !detail.comments.is_empty() { <span class="detail-tab-count">(detail.comments.len())</span> }
                        </a>
                        <a class=(tab_class(tab, Tab::Activity)) href=(format!("/?task={}&tab={}", detail.id, Tab::Activity.slug()))>(t(lang, Key::Activity))</a>
                        <a class=(tab_class(tab, Tab::Mail)) href=(format!("/?task={}&tab={}", detail.id, Tab::Mail.slug()))>(t(lang, Key::TabMail))</a>
                    </nav>

                    if tab == Tab::Task {
                        <div class="detail-fields">
                            <div class="detail-field detail-field-status">
                                <span class="detail-label">(t(lang, Key::Status))</span>
                                if may_write {
                                    <form class="status-form" method="post" action="/api/move_card">
                                        <input type="hidden" name="task_id" value=(detail.id.clone())>
                                        <input type="hidden" name="from_column_id" value=(detail.column.id.clone())>
                                        <span class=(class!("status-dot", "status-dot-done" if detail.column.is_done))></span>
                                        <select class="status-select" name="to_column_id" data-autosubmit="">
                                            for column in &detail.columns {
                                                <option value=(column.id.clone()) selected=(column.id == detail.column.id)>(column.name.clone())</option>
                                            }
                                        </select>
                                        (glyph::chevron(cx).await?)
                                    </form>
                                } else {
                                    <span class="field-box">
                                        <span class="status-dot"></span>
                                        <span class="field-text">(detail.column.name.clone())</span>
                                    </span>
                                }
                            </div>
                            <div class="detail-field detail-field-tag">
                                <span class="detail-label">(t(lang, Key::Project))</span>
                                if may_write {
                                    <form class="status-form" method="post" action="/api/set_task_tag">
                                        <input type="hidden" name="task_id" value=(detail.id.clone())>
                                        <select class="status-select" name="tag_id" data-autosubmit="" data-search="">
                                            for tag in &tags {
                                                <option value=(tag.id.clone()) selected=(Some(tag.id.as_str()) == detail.tag.as_ref().map(|t| t.id.as_str()))>(tag.name.clone())</option>
                                            }
                                        </select>
                                    </form>
                                } else {
                                    <span class="field-box">
                                        <span class="field-text">(
                                            detail.tag.as_ref().map(|tag| tag.name.clone()).unwrap_or_default()
                                        )</span>
                                    </span>
                                }
                            </div>
                            <div class="detail-field">
                                <span class="detail-label">(t(lang, Key::Deadline))</span>
                                (deadline_control(cx, &detail, today, zone, may_write, lang).await?)
                            </div>
                            <div class="detail-field detail-field-people">
                                <span class="detail-label">(format!("{} — {}", t(lang, Key::Assignees), detail.assignees.len()))</span>
                                <div class="detail-assignees">
                                    for person in &detail.assignees {
                                        (assignee_chip(cx, &detail.id, person, may_write, lang).await?)
                                    }
                                    <div class="spacer"></div>
                                    if may_write {
                                        (assignee_picker(cx, &detail.id, &unassigned, lang).await?)
                                    }
                                </div>
                            </div>
                        </div>
                        (refused(cx, "move_card", lang).await?)

                        <section class="detail-block">
                            <div class="detail-block-head">
                                <span class="detail-label">(t(lang, Key::Dependencies))</span>
                                <div class="spacer"></div>
                                if may_write {
                                    (link_picker(cx, &detail.id, &linkable, lang).await?)
                                }
                            </div>
                            if has_deps {
                                <div class="dep-list">
                                    for edge in &detail.blocked_by {
                                        (dep_row(cx, &detail.id, edge, Direction::BlockedBy, may_write, lang).await?)
                                    }
                                    for edge in &detail.blocks {
                                        (dep_row(cx, &detail.id, edge, Direction::Blocks, may_write, lang).await?)
                                    }
                                </div>
                            } else {
                                <p class="detail-prose detail-prose-empty">(t(lang, Key::NoDependencies))</p>
                            }
                        </section>
                    }
                </div>


                <div class="detail-body">
                    if tab == Tab::Task {


                    <section class="detail-block">
                        <span class="detail-label">(t(lang, Key::Description))</span>
                        (description_control(cx, &detail, may_write, lang).await?)
                    </section>
                    }

                    if tab == Tab::Subtasks {
                    <section class="detail-block detail-block-fill">
                        <div class="detail-block-head">
                            <span class="detail-label">(t(lang, Key::Subtasks))</span>
                            if !detail.subtasks.is_empty() {
                                <span class="detail-count">(format!("{}/{}", done_parts, detail.subtasks.len()))</span>
                            }
                            <div class="spacer"></div>
                            if may_write {
                                (adopt_picker(cx, &detail.id, &adoptable, lang).await?)
                            }
                        </div>
                        if may_write {
                            <form class="subtask-new" method="post" action="/api/create_subtask">
                                <input type="hidden" name="parent_id" value=(detail.id.clone())>
                                <input class="field-input subtask-new-input" type="text" name="title"
                                    placeholder=(t(lang, Key::NewSubtask)) required="" maxlength="200">
                                <button class="edit-save" type="submit">(t(lang, Key::AddSubtask))</button>
                            </form>
                        }
                        (refused(cx, "create_subtask", lang).await?)
                        (refused(cx, "set_parent", lang).await?)
                        if detail.subtasks.is_empty() {
                            <p class="detail-prose detail-prose-empty">(t(lang, Key::NoSubtasks))</p>
                        } else {
                            <div class="subtask-list">
                                for part in &detail.subtasks {
                                    (subtask_row(cx, part, &detail.columns, may_write, lang).await?)
                                }
                            </div>
                        }
                    </section>
                    }

                    if tab == Tab::Files {
                    <section class="detail-block">
                        <div class="detail-block-head">
                            <span class="detail-label">(t(lang, Key::Files))</span>
                            <span class="detail-count">(detail.files.len())</span>
                        </div>
                        if may_comment {
                            <form class="file-upload" method="post" action="/files" enctype="multipart/form-data">
                                <input type="hidden" name="task_id" value=(detail.id.clone())>
                                <label class="field-box file-upload-box">
                                    (glyph::plus(cx).await?)
                                    <span class="field-text file-upload-name">(t(lang, Key::File))</span>
                                    <input class="file-upload-input" type="file" name="file" accept=(accept) required="">
                                </label>
                            </form>
                            (refused(cx, "upload_file", lang).await?)
                        }
                        (refused(cx, "delete_file", lang).await?)
                        if !detail.files.is_empty() {
                            <div class="file-list">
                                for file in &detail.files {
                                    (file_chip(cx, &detail.id, file, &me, may_write, lang).await?)
                                }
                            </div>
                        }
                    </section>
                    }

                    if tab == Tab::Comments {
                    <section class="detail-block detail-block-fill">
                        <div class="detail-block-head">
                            <span class="detail-label">(t(lang, Key::Comments))</span>
                            <span class="detail-count">(detail.comments.len())</span>
                        </div>
                        <div class="comment-list">
                            for entry in &detail.comments {
                                (comment_row(cx, entry, zone).await?)
                            }
                        </div>
                    </section>
                    }

                    if tab == Tab::Activity {
                    <section class="detail-block detail-block-fill">
                        <span class="detail-label">(t(lang, Key::Activity))</span>
                        <div class="activity-list">
                            for entry in &detail.activity {
                                <div class="activity-line">
                                    <span class="activity-stamp">(entry.moment_in(zone))</span>
                                    <strong class="activity-who">(entry.actor.as_ref().map(|person| person.display_name.clone()).unwrap_or_else(|| "İzlek".to_string()))</strong>
                                    <span class="activity-what">(entry.sentence())</span>
                                </div>
                            }
                        </div>
                    </section>
                    }

                    if tab == Tab::Mail {
                    <section class="detail-block detail-block-fill">
                        <span class="detail-label">(t(lang, Key::Notifications))</span>
                        <div class="activity-list">
                            for line in &notifications {
                                <div class="activity-line">
                                    <span class="activity-stamp">(line.at.clone())</span>
                                    if let Some(rule_name) = &line.rule_name {
                                        <strong class="activity-who">(rule_name.clone())</strong>
                                    }
                                    <span class="activity-what">
                                        (line.happened.clone().unwrap_or_else(|| t(lang, Key::TaskGoneLabel).to_string()))
                                        // The outcome word only speaks when no mail was owed: once a
                                        // send exists its own state is the truth, and printing both
                                        // reads as a contradiction ("queued" beside a mail long sent).
                                        if line.sends.is_empty() {
                                            " — "
                                            <span class=(format!("rule-term rule-term-{}", line.outcome_kind))>(line.outcome.clone())</span>
                                        }
                                    </span>
                                    if !line.sends.is_empty() {
                                        <span class="activity-what">
                                            for send in &line.sends {
                                                <span class="rule-term">(send.recipient.clone())</span>
                                                <span class=(format!("rule-term rule-term-{}", send.state_kind))>
                                                    (if send.state_kind == "failed" || send.state_kind == "abandoned" {
                                                        send.last_error.clone().unwrap_or_else(|| send.state.clone())
                                                    } else if send.state_kind == "sent" {
                                                        format!("{} {}", send.state, send.sent_at.clone().unwrap_or_default())
                                                    } else {
                                                        send.state.clone()
                                                    })
                                                </span>
                                            }
                                        </span>
                                    }
                                </div>
                            }
                            if notifications.is_empty() {
                                <p class="rules-quiet">(t(lang, Key::NothingYet))</p>
                            }
                        </div>
                    </section>
                    }

                </div>

                if tab == Tab::Task {
                (refused(cx, "delete_task", lang).await?)

                <footer class="detail-foot">
                    if may_delete {
                        match cost {
                            Some(cost) => {
                                let freed = cost.frees.join(", ");
                                <details class="confirm-details" open=(confirm_delete)>
                                    <summary class="detail-delete">(glyph::bin(cx).await?)<span>(t(lang, Key::DeleteTask))</span></summary>
                                    <div class="confirm">
                                        <div class="confirm-title">(format!("{} — {}?", cost.task_key, cost.title))</div>
                                        <ul class="confirm-list">
                                            if cost.comment_count > 0 {
                                                <li>(if cost.comment_count == 1 { t(lang, Key::CommentGoesWithIt).to_string() } else { format!("{} {}", cost.comment_count, t(lang, Key::CommentsGoWithIt)) })</li>
                                            }
                                            if cost.subtask_count > 0 {
                                                <li>(if cost.subtask_count == 1 { t(lang, Key::SubtaskGoesWithIt).to_string() } else { format!("{} {}", cost.subtask_count, t(lang, Key::SubtasksGoWithIt)) })</li>
                                            }
                                            if cost.link_count > 0 {
                                                <li>(if cost.link_count == 1 { t(lang, Key::DependencyStopsApplying).to_string() } else { format!("{} {}", cost.link_count, t(lang, Key::DependenciesStopApplying)) })</li>
                                            }
                                            if !freed.is_empty() {
                                                <li>(format!("{freed} {}", t(lang, Key::StopsBeingBlocked)))</li>
                                            }
                                        </ul>
                                        <form class="detail-delete-form" method="post" action="/api/delete_task">
                                            <input type="hidden" name="task_id" value=(detail.id.clone())>
                                            <button class="detail-delete detail-delete-sure" type="submit">(t(lang, Key::DeleteTask))</button>
                                        </form>
                                    </div>
                                </details>
                            },
                            None => <p class="detail-quiet">(t(lang, Key::ThisTaskCannotBeDeleted))</p>,
                        }
                    }
                    <div class="spacer"></div>
                    <a class="quiet" href="/">(t(lang, Key::Close))</a>
                </footer>
                }

                if tab == Tab::Comments && may_comment {
                    <form class="comment-composer" method="post" action="/api/post_comment">
                        <input type="hidden" name="task_id" value=(detail.id.clone())>
                        <textarea class="detail-textarea comment-input" name="body" rows="3" placeholder=(t(lang, Key::WriteAComment)) required=""></textarea>
                        <div class="comment-row">
                            <div class="spacer"></div>
                            <button class="comment-post" type="submit">(t(lang, Key::Comment))</button>
                        </div>
                    </form>
                    (refused(cx, "post_comment", lang).await?)
                }
            </div>
        </div>
        (datepicker_script(cx, lang).await?)
        (escape_closes(cx).await?)
    }
}

/// The house audio player: play toggle, clock, seek bar over a hidden
/// `<audio>` — native controls live in the browser's own chrome, ignore the
/// theme, and swallow `Escape` while focused, so the viewer draws its own.
/// Wiring is per-player and idempotent (`data-wired`), re-run on
/// `izlek:wire` so a player arriving in a soft page swap — where inline
/// scripts never execute — still gets its controls.
async fn audio_player_script(cx: &Cx) -> Result {
    use topcoat::view::Unescaped;
    const JS: &str = "\
        (function () { \
            if (window.__izlekAudioWired) { return; } \
            window.__izlekAudioWired = true; \
            function wireAll() { document.querySelectorAll('.audio-player').forEach(wirePlayer); } \
            function wirePlayer(player) { \
            if (player.dataset.wired) { return; } \
            window.__izlekOwn(player, [], ['data-wired']); \
            player.dataset.wired = '1'; \
            var audio = player.querySelector('.audio-el'); \
            var play = player.querySelector('.audio-play'); \
            var seek = player.querySelector('.audio-seek'); \
            var now = player.querySelector('.audio-now'); \
            var dur = player.querySelector('.audio-dur'); \
            function clock(s) { \
                if (!isFinite(s)) { return '0:00'; } \
                var m = Math.floor(s / 60); \
                var r = Math.floor(s % 60); \
                return m + ':' + (r < 10 ? '0' : '') + r; \
            } \
            play.addEventListener('click', function () { \
                if (audio.paused) { audio.play(); } else { audio.pause(); } \
            }); \
            audio.addEventListener('play', function () { \
                window.__izlekOwn(player, ['audio-playing'], []); \
                play.setAttribute('aria-label', play.getAttribute('data-pause')); \
            }); \
            audio.addEventListener('pause', function () { \
                player.classList.remove('audio-playing'); \
                play.setAttribute('aria-label', play.getAttribute('data-play')); \
            }); \
            audio.addEventListener('loadedmetadata', function () { dur.textContent = clock(audio.duration); }); \
            audio.addEventListener('timeupdate', function () { \
                now.textContent = clock(audio.currentTime); \
                if (audio.duration) { seek.value = audio.currentTime / audio.duration * 1000; } \
            }); \
            seek.addEventListener('input', function () { \
                if (audio.duration) { audio.currentTime = seek.value / 1000 * audio.duration; } \
            }); \
            } \
            wireAll(); \
            document.addEventListener('izlek:wire', wireAll); \
        })();";
    view! { cx => <script>(Unescaped::new_unchecked(JS))</script> }
}

/// The in-app file viewer: `<img>`/`<video>`/`<audio>`/`<object>` per
/// [`crate::files::ViewerKind`], opened over the task modal via
/// `?task=<id>&file=<id>` — the same query-param overlay pattern as
/// [`task_modal`] itself off `board.rs`. Renders nothing (not even a scrim)
/// when the file is not this task's, or is not this workspace's, or carries a
/// mime type with no viewer element: a filename's own link only ever points
/// here when [`crate::files::viewer_kind`] already said yes, but a hand-edited
/// query string gets the same silent no as a stale or foreign id.
pub async fn file_viewer_modal(
    cx: &Cx,
    task_id: &str,
    file_id: &str,
    tab: Tab,
    sheet_index: usize,
    row_page: usize,
    column_page: usize,
) -> Result {
    let user = match require_user(cx).await {
        Ok(user) => user,
        Err(_) => return view! { cx => },
    };
    let store = accounts(cx).store().clone();
    if task_of(store.as_ref(), &user, task_id).await.is_err() {
        return view! { cx => };
    }
    let Ok(Some(attachment)) = store.attachment(file_id).await else {
        return view! { cx => };
    };
    if attachment.task_id != task_id {
        return view! { cx => };
    }
    let Some(kind) = crate::files::viewer_kind(&attachment.mime_type) else {
        return view! { cx => };
    };
    let lang = Lang::from_code(&user.language);
    // Closing a file returns to the panel exactly as it stood, tab and all:
    // a viewer is opened from a section, not from the task at large.
    let close_href = format!("/?task={task_id}&tab={}", tab.slug());
    let stamp = attachment_stamp(&attachment);
    let src = format!("/files/{file_id}?v={stamp}");
    let download_href = format!("/files/{file_id}?dl=1&v={stamp}");
    let name = attachment.file_name.clone();
    // A workbook is the one viewer whose bytes are read here rather than
    // handed to an element; every other kind streams from `/files/{id}` and
    // never loads the file into the page's own request.
    // Reading a workbook is the one piece of real CPU a page render does, so
    // it happens off the runtime's own threads: a big book is most of a
    // second, and a second of a worker thread is every other request waiting.
    let sheet = if kind == crate::files::ViewerKind::Sheet {
        match store.attachment_bytes(file_id).await {
            Ok(Some(bytes)) => tokio::task::spawn_blocking(move || {
                crate::sheet::read(bytes, sheet_index, row_page, column_page)
            })
            .await
            .ok()
            .flatten(),
            _ => None,
        }
    } else {
        None
    };
    view! {
        cx =>
        <div class="modal-scrim viewer-scrim">
            <a class="viewer-close" href=(close_href.clone()) aria-label=(t(lang, Key::CloseTheFile))></a>
            <div class="modal viewer" tabindex="-1">
                <header class="detail-head">
                    <span class="detail-headline"><span class="detail-key">(name.clone())</span></span>
                    <a class="quiet" href=(download_href)>(t(lang, Key::Download))</a>
                    <span class="detail-esc">(t(lang, Key::Esc))</span>
                    <a class="detail-close" href=(close_href) aria-label=(t(lang, Key::CloseTheFile))>(glyph::cross(cx).await?)</a>
                </header>
                <div class="viewer-body">
                    match kind {
                        crate::files::ViewerKind::Image => <img class="viewer-media" src=(src) alt=(name)>,
                        crate::files::ViewerKind::Video => <video class="viewer-media" src=(src) controls=""></video>,
                        crate::files::ViewerKind::Audio => <div class="audio-player">
                            <button type="button" class="audio-play" aria-label=(t(lang, Key::Play))
                                data-play=(t(lang, Key::Play)) data-pause=(t(lang, Key::Pause))>
                                (glyph::play(cx).await?)
                                (glyph::pause(cx).await?)
                            </button>
                            <span class="audio-time audio-now">"0:00"</span>
                            <input class="audio-seek" type="range" min="0" max="1000" value="0" aria-label=(name.clone())>
                            <span class="audio-time audio-dur">"0:00"</span>
                            <audio class="audio-el" src=(src) preload="metadata"></audio>
                            (audio_player_script(cx).await?)
                        </div>,
                        crate::files::ViewerKind::Pdf => <object class="viewer-media viewer-pdf" data=(src) type="application/pdf"></object>,
                        crate::files::ViewerKind::Sheet => (sheet_view(cx, sheet, task_id, file_id, tab, lang).await?),
                    }
                </div>
            </div>
        </div>
    }
}

/// One window of one sheet as a table: the tab strip when the book has more
/// than one sheet, the grid with its column letters and row numbers, and the
/// two pagers that move the window. A workbook no reader here understands
/// says so in one line — the header's download link is still the whole file.
async fn sheet_view(
    cx: &Cx,
    sheet: Option<crate::sheet::Sheet>,
    task_id: &str,
    file_id: &str,
    tab: Tab,
    lang: Lang,
) -> Result {
    use crate::sheet::{COLUMNS_PER_PAGE, ROWS_PER_PAGE, column_name};

    let Some(sheet) = sheet else {
        return view! { cx => <p class="sheet-note">(t(lang, Key::ThisFileWillNotOpen))</p> };
    };
    let width = sheet.rows.iter().map(Vec::len).max().unwrap_or(0);
    let columns: Vec<String> = (0..width)
        .map(|column| column_name(sheet.first_column + column))
        .collect();
    let row_page = sheet.first_row / ROWS_PER_PAGE;
    let column_page = sheet.first_column / COLUMNS_PER_PAGE;
    // Every link out of here is the same page with one number moved, so the
    // window a link lands on is the window its own numbers name.
    let at = |sheet_index: usize, rows: usize, columns: usize| {
        format!(
            "/?task={task_id}&tab={}&file={file_id}&sheet={sheet_index}&rows={rows}&cols={columns}",
            tab.slug()
        )
    };
    // The tab strip is a row of links, not a control: switching sheets is a
    // navigation like every other overlay state on this page, and it starts
    // that sheet at its own first window.
    let tabs: Vec<(String, String, &'static str)> = sheet
        .names
        .iter()
        .enumerate()
        .map(|(index, name)| {
            (
                name.clone(),
                at(index, 0, 0),
                if index == sheet.index {
                    "sheet-tab sheet-tab-on"
                } else {
                    "sheet-tab"
                },
            )
        })
        .collect();
    let rows_back = (row_page > 0).then(|| at(sheet.index, row_page - 1, column_page));
    let rows_on = sheet
        .more_rows
        .then(|| at(sheet.index, row_page + 1, column_page));
    let columns_back = (column_page > 0).then(|| at(sheet.index, row_page, column_page - 1));
    let columns_on = sheet
        .more_columns
        .then(|| at(sheet.index, row_page, column_page + 1));
    // "51–100 / 5000" down the rows, "M–X / 40" across the columns: the
    // window's own edges in the same vocabulary the grid labels its gutters
    // with, counted against the whole sheet where the file's own size can be
    // trusted and left uncounted where it cannot.
    let row_range = match sheet.total_rows {
        Some(total) => format!("{}–{} / {total}", sheet.first_row + 1, sheet.last_row()),
        None => format!("{}–{}", sheet.first_row + 1, sheet.last_row()),
    };
    let first_letter = column_name(sheet.first_column);
    let last_letter = column_name(sheet.last_column().saturating_sub(1));
    let column_range = match sheet.total_columns {
        Some(total) => format!("{first_letter}–{last_letter} / {total}"),
        None => format!("{first_letter}–{last_letter}"),
    };
    view! {
        cx =>
        <div class="viewer-sheet">
            if tabs.len() > 1 {
                <nav class="sheet-tabs">
                    for tab in &tabs {
                        <a class=(tab.2) href=(tab.1.clone())>(tab.0.clone())</a>
                    }
                </nav>
            }
            <div class="sheet-grid">
                <table class="sheet-table">
                    <thead>
                        <tr>
                            <th class="sheet-gutter"></th>
                            for column in &columns {
                                <th>(column.clone())</th>
                            }
                        </tr>
                    </thead>
                    <tbody>
                        for (index, row) in sheet.rows.iter().enumerate() {
                            <tr>
                                <th class="sheet-gutter">(format!("{}", sheet.first_row + index + 1))</th>
                                for column in 0..width {
                                    <td>(row.get(column).cloned().unwrap_or_default())</td>
                                }
                            </tr>
                        }
                    </tbody>
                </table>
            </div>
            <div class="sheet-foot">
                <div class="sheet-pager">
                    <span class="sheet-range">(row_range)</span>
                    if let Some(href) = rows_back {
                        <a class="sheet-step sheet-step-up" href=(href) aria-label=(t(lang, Key::Previous))>(glyph::chevron(cx).await?)</a>
                    } else {
                        <span class="sheet-step sheet-step-up sheet-step-off">(glyph::chevron(cx).await?)</span>
                    }
                    if let Some(href) = rows_on {
                        <a class="sheet-step" href=(href) aria-label=(t(lang, Key::Next))>(glyph::chevron(cx).await?)</a>
                    } else {
                        <span class="sheet-step sheet-step-off">(glyph::chevron(cx).await?)</span>
                    }
                </div>
                <div class="sheet-pager">
                    <span class="sheet-range">(column_range)</span>
                    if let Some(href) = columns_back {
                        <a class="sheet-step sheet-step-left" href=(href) aria-label=(t(lang, Key::Previous))>(glyph::chevron(cx).await?)</a>
                    } else {
                        <span class="sheet-step sheet-step-left sheet-step-off">(glyph::chevron(cx).await?)</span>
                    }
                    if let Some(href) = columns_on {
                        <a class="sheet-step sheet-step-right" href=(href) aria-label=(t(lang, Key::Next))>(glyph::chevron(cx).await?)</a>
                    } else {
                        <span class="sheet-step sheet-step-right sheet-step-off">(glyph::chevron(cx).await?)</span>
                    }
                </div>
            </div>
        </div>
    }
}

/// The "New task" popup: a house modal wearing the same scrim/panel chrome
/// as [`task_modal`], with a form posting straight to `/api/create_task`.
/// Wired off the board top bar's "New task" button as `/?new=1`.
pub async fn new_task_modal(cx: &Cx, columns: &[(String, String)], lang: Lang) -> Result {
    view! {
        cx =>
        <div class="modal-scrim">
            <div class="modal modal-new-task" tabindex="-1">
                <header class="detail-head">
                    <span class="detail-headline">(t(lang, Key::NewTask))</span>
                    <span class="detail-esc">(t(lang, Key::Esc))</span>
                    <a class="detail-close" href="/" aria-label=(t(lang, Key::CloseThisTask))>(glyph::cross(cx).await?)</a>
                </header>
                <form class="new-task-form" method="post" action="/api/create_task">
                    <label class="field">
                        <span class="field-label">(t(lang, Key::Title))</span>
                        <input class="field-input" type="text" name="title" autocomplete="off" required="">
                    </label>
                    <div class="new-task-row">
                        <label class="field">
                            <span class="field-label">(t(lang, Key::Status))</span>
                            <span class="field-box">
                                <select class="status-select" name="column_id">
                                    for column in columns {
                                        <option value=(column.0.clone())>(column.1.clone())</option>
                                    }
                                </select>
                                (glyph::chevron(cx).await?)
                            </span>
                        </label>
                        <div class="field">
                            <span class="field-label">(t(lang, Key::Deadline))</span>
                            <div class="edit edit-pop datepick-pop">
                                <input class="edit-toggle" type="checkbox" id="new-task-deadline">
                                <label class="field-box edit-view edit-hit" for="new-task-deadline">
                                    (glyph::calendar(cx).await?)
                                    <span class="field-text datepick-label" data-empty=(t(lang, Key::NoDeadline))>(t(lang, Key::NoDeadline))</span>
                                </label>
                                <div class="edit-form pop-panel datepick-panel">
                                    (datepicker_grid(cx, "deadline", "", false, lang).await?)
                                    <div class="datepick-time">
                                        <select class="field-input" name="clock_hour" aria-label=(t(lang, Key::ClockHour)) data-search="">
                                            <option value="">("--")</option>
                                            for hour in 0u8..24 {
                                                <option value=(format!("{hour:02}"))>(format!("{hour:02}"))</option>
                                            }
                                        </select>
                                        <span class="datepick-colon">(":")</span>
                                        <select class="field-input" name="clock_minute" aria-label=(t(lang, Key::ClockMinute)) data-search="">
                                            <option value="">("--")</option>
                                            for minute in 0u8..60 {
                                                <option value=(format!("{minute:02}"))>(format!("{minute:02}"))</option>
                                            }
                                        </select>
                                    </div>
                        </div>
                    </div>
                        </div>
                    </div>
                    <label class="field">
                        <span class="field-label">(t(lang, Key::Description))</span>
                        <textarea class="detail-textarea" name="description" rows="4"></textarea>
                    </label>
                    (refused(cx, "create_task", lang).await?)
                    <div class="new-task-foot">
                        <div class="spacer"></div>
                        <a class="quiet" href="/">(t(lang, Key::Cancel))</a>
                        <button class="primary" type="submit">(t(lang, Key::NewTask))</button>
                    </div>
                </form>
            </div>
        </div>
        (datepicker_script(cx, lang).await?)
        (escape_closes(cx).await?)
    }
}
