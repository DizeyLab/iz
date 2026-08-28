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
use izlek_core::detail::{Comment, DeletionCost, DependencyEdge, TaskDetail, TaskFacts, moment_label_in, parse_zone};
use izlek_core::store::{Store, StoreError, User};
use serde::{Deserialize, Serialize};
use time::{Date, UtcOffset};
use topcoat::Result;
use topcoat::context::Cx;
use topcoat::router::content::{Form, Json};
use topcoat::router::request::headers;
use topcoat::router::{HeaderName, StatusCode, header, route};
use topcoat::view::{class, view};

use crate::i18n::{Key, Lang, t};
use crate::server::{Refusal, accounts, mail, refusal_of, require_user, require_writer};

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
}

impl From<&User> for Me {
    fn from(user: &User) -> Self {
        Me {
            id: user.id.clone(),
            display_name: user.display_name.clone(),
            email: user.email.clone(),
            role: user.role,
            language: user.language.clone(),
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
    pub may_write: bool,
    pub may_comment: bool,
    pub may_delete: bool,
    pub allowed_file_types: Vec<String>,
    pub attachment_limit_mb: u64,
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
async fn task_of(store: &dyn Store, user: &User, task_id: &str) -> std::result::Result<TaskFacts, Refusal> {
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
async fn writer_and_task(cx: &Cx, task_id: &str) -> std::result::Result<(User, TaskFacts), Refusal> {
    let user = require_writer(cx).await?;
    let store = accounts(cx).store().clone();
    let facts = task_of(store.as_ref(), &user, task_id).await?;
    Ok((user, facts))
}

/// A 303 back to wherever the form was posted from, carrying `refusal` as the
/// body for `carry_refusal_on_redirect` to read — the same shape `auth.rs`
/// uses for every one of its mutating calls.
type Redirect = Result<(StatusCode, [(HeaderName, String); 1], Json<Option<Refusal>>)>;

fn back_to(cx: &Cx) -> String {
    headers(cx)
        .get(header::REFERER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("/")
        .to_string()
}

fn redirect(cx: &Cx, refusal: Option<Refusal>) -> Redirect {
    Ok((StatusCode::SEE_OTHER, [(header::LOCATION, back_to(cx))], Json(refusal)))
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
async fn load_snapshot(cx: &Cx, task_id: &str) -> Result<std::result::Result<DetailSnapshot, Refusal>> {
    use izlek_core::detail::load;
    use time::OffsetDateTime;

    let user = match require_user(cx).await {
        Ok(user) => user,
        Err(refusal) => return Ok(Err(refusal)),
    };
    let store = accounts(cx).store().clone();

    let Some(detail) = load(store.as_ref(), &user.workspace_id, task_id).await? else {
        return Ok(Err(Refusal::NotFound));
    };

    // What the picker may offer: this board's other tasks, minus the ones
    // already on either end of a live link with this one. A cleared edge is
    // not a link any more, so its task comes back to the picker.
    let mut linkable = Vec::new();
    if let Some(board) = store.board(&user.workspace_id).await? {
        let taken: Vec<&str> = detail
            .blocked_by
            .iter()
            .chain(detail.blocks.iter())
            .filter(|edge| edge.cleared_at.is_none())
            .map(|edge| edge.task_id.as_str())
            .collect();
        for task in store.tasks_for_board(&board.id).await? {
            if task.id == detail.id || taken.contains(&task.id.as_str()) {
                continue;
            }
            linkable.push(LinkTarget {
                id: task.id,
                task_key: task.task_key,
                title: task.title,
            });
        }
        // Sorted by id, not `task_key`: the key's tail is a random ULID
        // suffix now, not a counter, so only the id (a ULID itself) still
        // orders these by creation time.
        linkable.sort_by(|a, b| a.id.cmp(&b.id));
    }

    let may_write = user.role.can_write_tasks();

    const MB: u64 = 1024 * 1024;
    let workspace = store.workspace().await?;
    let allowed_file_types = workspace
        .as_ref()
        .map(|workspace| workspace.allowed_file_types.clone())
        .unwrap_or_default();
    let attachment_limit_mb = workspace.map(|workspace| workspace.attachment_limit_bytes / MB).unwrap_or(0);

    Ok(Ok(DetailSnapshot {
        detail,
        linkable,
        may_write,
        may_comment: user.role.can_comment(),
        may_delete: may_write,
        allowed_file_types,
        attachment_limit_mb,
        me: Me::from(&user),
        today: OffsetDateTime::now_utc().date(),
        zone: parse_zone(&user.timezone),
    }))
}

#[route(POST "/api/fetch_task")]
async fn fetch_task(cx: &Cx, Form(input): Form<TaskIdForm>) -> Result<Json<std::result::Result<DetailSnapshot, Refusal>>> {
    Ok(Json(load_snapshot(cx, &input.task_id).await?))
}

/// Saves the title, the description and the deadline. Status is not here: the
/// column a task sits in is changed by moving it, a call `board.rs` owns.
#[route(POST "/api/save_task")]
async fn save_task(cx: &Cx, Form(input): Form<SaveTaskForm>) -> Redirect {
    use time::OffsetDateTime;
    use time::macros::format_description;

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
    let deadline = match input.deadline.as_deref() {
        None => facts.row.deadline,
        Some(raw) => match raw.trim() {
            "" => None,
            day => match Date::parse(day, format_description!("[year]-[month]-[day]")) {
                Ok(day) => Some(day),
                Err(_) => return redirect(cx, Some(Refusal::BadDeadline)),
            },
        },
    };

    let store = accounts(cx).store().clone();
    let activity_ids = store
        .save_task(&input.task_id, &title, &description, deadline, &user.id, OffsetDateTime::now_utc())
        .await?;
    for activity_id in activity_ids {
        mail(cx).after_activity(store.clone(), activity_id);
    }
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
        .record_activity(&input.task_id, Some(&actor.id), &ActivityKind::Assigned, &person.display_name, OffsetDateTime::now_utc())
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
        .record_activity(&input.task_id, Some(&actor.id), &ActivityKind::Unassigned, &person.display_name, OffsetDateTime::now_utc())
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
        .record_activity(&input.task_id, Some(&actor.id), &ActivityKind::Linked, &other.row.task_key, now)
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
        .record_activity(&input.task_id, Some(&actor.id), &ActivityKind::Unlinked, &other.row.task_key, now)
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

    let written = store.add_comment(&input.task_id, &user.id, body, OffsetDateTime::now_utc()).await?;
    mail(cx).after_activity(store, written.activity_id);
    redirect(cx, None)
}

/// What a delete would take with it, for the confirmation step. Reads only.
#[route(POST "/api/what_delete_costs")]
async fn what_delete_costs(cx: &Cx, Form(input): Form<TaskIdForm>) -> Result<Json<std::result::Result<DeletionCost, Refusal>>> {
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
    let deletion = store.delete_task(&input.task_id, &user.id, OffsetDateTime::now_utc()).await?;
    // A blocker being deleted unblocks whatever was waiting only on it, which
    // is the same news as the blocker finishing. The freeing is committed;
    // the send is a separate step, off the request.
    if let Some(freeing) = deletion.event {
        mail(cx).after_freeing(freeing, deletion.freed);
    }
    mail(cx).after_activity(store, deletion.activity_id);
    // Deleted; the referring `/?task=<id>` no longer names anything, so land
    // on the board itself rather than reopening a dead modal.
    Ok((StatusCode::SEE_OTHER, [(header::LOCATION, "/".to_string())], Json(None)))
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

/// A person as a circle. Ported from `izlek-web/src/board.rs`'s `Avatar`; that
/// component has not crossed over into this crate yet, so this is a private
/// copy rather than a shared one.
async fn avatar(cx: &Cx, person: &Person, extra: &str) -> Result {
    let initials = person.initials();
    let tone = person.id.bytes().fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32)) % 5;
    view! {
        cx =>
        <span class=(class!("avatar", format!("avatar-tone-{tone}"), extra))>
            (initials)
        </span>
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
    let prose = if empty { no_description.clone() } else { task.description.clone() };

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

async fn deadline_control(cx: &Cx, task: &TaskDetail, today: Date, may_write: bool, lang: Lang) -> Result {
    let overdue = task.is_overdue(today);
    let label = match task.deadline_parts(today) {
        Some(parts) if parts.state == DeadlineState::Overdue => format!("{} · {}", parts.date, t(lang, Key::Overdue)),
        Some(parts) => parts.date,
        None => t(lang, Key::NoDeadline).to_string(),
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
                    (datepicker_grid(cx, "deadline", &input_value, lang).await?)
                    <div class="edit-row">
                        <button class="edit-save" type="submit">(t(lang, Key::Save))</button>
                        <label class="edit-cancel" for=(toggle)>(t(lang, Key::Cancel))</label>
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
async fn datepicker_grid(cx: &Cx, name: &str, value: &str, lang: Lang) -> Result {
    view! {
        cx =>
        <input class="datepick-input" type="hidden" name=(name.to_string()) value=(value.to_string())>
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
async fn datepicker_script(cx: &Cx, lang: Lang) -> Result {
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
            function render(panel) {{\
                var input = panel.querySelector('.datepick-input');\
                var sel = parseYmd(input.value);\
                var t = todayYmd();\
                if (!panel.dataset.year) {{ var base = sel || t; panel.dataset.year = base.y; panel.dataset.month = base.m; }}\
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
                input.value = ymd ? (ymd.y + '-' + pad(ymd.m) + '-' + pad(ymd.d)) : '';\
                var pop = panel.closest('.datepick-pop');\
                var label = pop.querySelector('.datepick-label');\
                if (label) {{ label.textContent = ymd ? (MONTHS[ymd.m - 1].slice(0, 3) + ' ' + pad(ymd.d)) : label.dataset.empty; }}\
                var toggle = pop.querySelector('.edit-toggle');\
                if (toggle) {{ toggle.checked = false; }}\
            }}\
            document.addEventListener('change', function(e) {{\
                var toggle = e.target.closest('.datepick-pop > .edit-toggle');\
                if (!toggle || !toggle.checked) {{ return; }}\
                var panel = toggle.closest('.datepick-pop').querySelector('.datepick-panel');\
                if (panel) {{ render(panel); }}\
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
                if (openPop && !openPop.closest('.datepick-pop').contains(e.target)) {{ openPop.checked = false; }}\
            }}, true);\
        }})();"
    );
    view! { cx => <script>(Unescaped::new_unchecked(js))</script> }
}

async fn assignee_chip(cx: &Cx, task_id: &str, person: &Person, may_write: bool, lang: Lang) -> Result {
    let remove_title = crate::i18n::take_off_this_task(lang, &person.display_name);
    view! {
        cx =>
        <span class="assignee-chip">
            (avatar(cx, person, "avatar-sm").await?)
            <span class="assignee-name">(person.display_name.clone())</span>
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
                                (avatar(cx, person, "avatar-sm").await?)
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

async fn dep_row(cx: &Cx, task_id: &str, edge: &DependencyEdge, direction: Direction, may_write: bool, lang: Lang) -> Result {
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
        format!("/?task={task_id}&file={}", file.id)
    } else {
        format!("/files/{}", file.id)
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
            (avatar(cx, &comment.author, "avatar-lg").await?)
            <div class="comment-said">
                <div class="comment-head">
                    <span class="comment-who">(comment.author.display_name.clone())</span>
                    <span class="comment-when">(moment_label_in(comment.at, zone))</span>
                </div>
                <div class="comment-body">(comment.body.clone())</div>
            </div>
        </div>
    }
}

/// The task modal's markup: title, description, assignees, deadline,
/// dependencies, files, comments, activity and delete, exactly as the
/// artboard draws them. Wiring `?task=<id>` on the board page is a later
/// integration slice — this only renders the fragment.
pub async fn task_modal(cx: &Cx, task_id: &str, confirm_delete: bool) -> Result {
    let snapshot = match load_snapshot(cx, task_id).await? {
        Ok(snapshot) => snapshot,
        Err(refusal) => {
            // A refused snapshot can still have a signed-in user behind it —
            // a gone task id is the common case — so the language is read
            // the same way `board.rs`'s query-refusal branch does, English
            // only when there truly is nobody signed in to read one off of.
            let lang = require_user(cx).await.map(|user| Lang::from_code(&user.language)).unwrap_or(Lang::En);
            return view! {
                cx =>
                <div class="modal-scrim">
                    <div class="modal"><p class="modal-note">(refusal.message_in(lang))</p></div>
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
        may_write,
        may_comment,
        may_delete,
        allowed_file_types,
        attachment_limit_mb: _,
    } = snapshot;
    let lang = Lang::from_code(&me.language);

    let unassigned: Vec<Person> = detail.unassigned().cloned().collect();
    let has_deps = !detail.blocked_by.is_empty() || !detail.blocks.is_empty();
    let accept = (!allowed_file_types.is_empty())
        .then(|| allowed_file_types.iter().map(|kind| format!(".{kind}")).collect::<Vec<_>>().join(","))
        .unwrap_or_default();

    // The delete confirmation is computed eagerly rather than fetched on
    // demand: there is no script here to hold the intermediate "did they
    // click delete yet" state, so the cost is already known by the time the
    // disclosure opens.
    let cost = if may_delete { accounts(cx).store().deletion_cost(&detail.id).await? } else { None };

    view! {
        cx =>
        <div class="modal-scrim">
            <div class="modal" tabindex="-1">
                <header class="detail-head">
                    <div class="detail-headline">
                        <span class="detail-key">(detail.task_key.clone())</span>
                        (title_control(cx, &detail, may_write, lang).await?)
                    </div>
                    <span class="detail-esc">(t(lang, Key::Esc))</span>
                    <a class="detail-close" href="/" aria-label=(t(lang, Key::CloseThisTask))>(glyph::cross(cx).await?)</a>
                </header>

                <div class="detail-fields">
                    <div class="detail-field">
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
                        (refused(cx, "move_card", lang).await?)
                    </div>
                    <div class="detail-field">
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
                    <div class="detail-field">
                        <span class="detail-label">(t(lang, Key::Deadline))</span>
                        (deadline_control(cx, &detail, today, may_write, lang).await?)
                    </div>
                </div>

                <section class="detail-block">
                    <span class="detail-label">(t(lang, Key::Description))</span>
                    (description_control(cx, &detail, may_write, lang).await?)
                </section>

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

                <section class="detail-block">
                    <div class="detail-block-head">
                        <span class="detail-label">(t(lang, Key::Files))</span>
                        <span class="detail-count">(detail.files.len())</span>
                    </div>
                    if !detail.files.is_empty() {
                        <div class="file-list">
                            for file in &detail.files {
                                (file_chip(cx, &detail.id, file, &me, may_write, lang).await?)
                            }
                        </div>
                    }
                    (refused(cx, "delete_file", lang).await?)
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
                </section>

                <section class="detail-block">
                    <div class="detail-block-head">
                        <span class="detail-label">(t(lang, Key::Comments))</span>
                        <span class="detail-count">(detail.comments.len())</span>
                    </div>
                    <div class="comment-list">
                        for entry in &detail.comments {
                            (comment_row(cx, entry, zone).await?)
                        }
                    </div>
                    if may_comment {
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
                </section>

                <section class="detail-block">
                    <span class="detail-label">(t(lang, Key::Activity))</span>
                    <div class="activity-list">
                        for entry in &detail.activity {
                            <div class="activity-line">
                                <span class="activity-stamp">(entry.moment_in(zone))</span>
                                <strong class="activity-who">(entry.actor.as_ref().map(|person| person.display_name.clone()).unwrap_or_else(|| "Izlek".to_string()))</strong>
                                <span class="activity-what">(entry.sentence())</span>
                            </div>
                        }
                    </div>
                </section>

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
                player.classList.add('audio-playing'); \
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
pub async fn file_viewer_modal(cx: &Cx, task_id: &str, file_id: &str) -> Result {
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
    let close_href = format!("/?task={task_id}");
    let src = format!("/files/{file_id}");
    let download_href = format!("/files/{file_id}?dl=1");
    let name = attachment.file_name.clone();
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
                                    (datepicker_grid(cx, "deadline", "", lang).await?)
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
