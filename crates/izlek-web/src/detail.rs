//! The task detail modal, from the TaskDetail artboard.
//!
//! One server call brings the whole screen: the task, its assignees, both
//! directions of its dependencies, its comments and its activity. Every change
//! is its own call, and every one of them checks the task belongs to the
//! asker's workspace before it does anything — a task id in a form is an
//! authorization question, not a validation one.

use izlek_core::board::Person;
use izlek_core::detail::{Comment, DeletionCost, DependencyEdge, TaskDetail};
use leptos::either::Either;
use leptos::prelude::*;
use serde::{Deserialize, Serialize};
use time::Date;

use crate::auth::{Me, Refusal};
use crate::board::Avatar;

/// A task this board could be linked to: enough to name it in the picker and
/// nothing more.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkTarget {
    pub id: String,
    pub task_key: String,
    pub title: String,
}

/// One task detail's worth of state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DetailSnapshot {
    pub detail: TaskDetail,
    pub me: Me,
    pub today: Date,
    /// What the "Link a task" picker may offer — never this task, never one it
    /// is already linked to.
    pub linkable: Vec<LinkTarget>,
    pub may_write: bool,
    pub may_comment: bool,
    /// Whether this person may delete the task. A writer may.
    pub may_delete: bool,
    /// Extensions the upload form's `accept` attribute may offer. Empty means
    /// every type — the same "empty means everything" the store's limits use.
    pub allowed_file_types: Vec<String>,
    pub attachment_limit_mb: u64,
}

/// Which way round a dependency runs, as it travels in a form.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// The other task must finish before this one can start.
    BlockedBy,
    /// The other task is waiting on this one.
    Blocks,
}

#[cfg(feature = "ssr")]
pub(crate) mod guard {
    use crate::auth::Refusal;
    use izlek_core::detail::TaskFacts;
    use izlek_core::store::{Store, User};

    /// The task, if this person's workspace is the one holding it. A task in
    /// another workspace is not found rather than forbidden: the answer says
    /// nothing about whether the id is real.
    ///
    /// Takes the store rather than reading it off leptos context, so a plain
    /// axum handler with no leptos owner tree — `crate::files`'s upload and
    /// download routes — can call this the same way a server function does.
    pub async fn task_of(
        store: &dyn Store,
        user: &User,
        task_id: &str,
    ) -> Result<TaskFacts, Refusal> {
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
    pub async fn writer_and_task(task_id: &str) -> Result<(User, TaskFacts), Refusal> {
        let user = crate::server::require_writer().await?;
        let store = crate::server::accounts().store().clone();
        let facts = task_of(store.as_ref(), &user, task_id).await?;
        Ok((user, facts))
    }
}

/// Everything one task detail shows. A Viewer may read it; what they may not do
/// is refused in the calls below, not merely hidden from them here.
#[server]
pub async fn fetch_task(task_id: String) -> Result<Result<DetailSnapshot, Refusal>, ServerFnError> {
    use crate::server::{accounts, require_user};
    use izlek_core::detail::load;
    use time::OffsetDateTime;

    let user = match require_user().await {
        Ok(user) => user,
        Err(refusal) => return Ok(Err(refusal)),
    };
    let store = accounts().store().clone();
    let fail = |e: izlek_core::store::StoreError| ServerFnError::new(e.to_string());

    let Some(detail) = load(store.as_ref(), &user.workspace_id, &task_id)
        .await
        .map_err(fail)?
    else {
        return Ok(Err(Refusal::NotFound));
    };

    // What the picker may offer: this board's other tasks, minus the ones
    // already on either end of a live link with this one. A cleared edge is
    // not a link any more, so its task comes back to the picker — the store's
    // upsert revives that row rather than duplicating it.
    let mut linkable = Vec::new();
    if let Some(board) = store.board(&user.workspace_id).await.map_err(fail)? {
        let taken: Vec<&str> = detail
            .blocked_by
            .iter()
            .chain(detail.blocks.iter())
            .filter(|edge| edge.cleared_at.is_none())
            .map(|edge| edge.task_id.as_str())
            .collect();
        for task in store.tasks_for_board(&board.id).await.map_err(fail)? {
            if task.id == detail.id || taken.contains(&task.id.as_str()) {
                continue;
            }
            linkable.push(LinkTarget {
                id: task.id,
                task_key: task.task_key,
                title: task.title,
            });
        }
        linkable.sort_by(|a, b| a.task_key.cmp(&b.task_key));
    }

    // A writer may delete. The delete is soft and the confirmation says what
    // goes with it, which is what makes that safe — not a role.
    let may_write = user.role.can_write_tasks();

    // The upload form's own limits, read the same way Settings reads them —
    // megabytes on the screen, bytes in the store.
    const MB: u64 = 1024 * 1024;
    let workspace = store.workspace().await.map_err(fail)?;
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
        may_write,
        may_comment: user.role.can_comment(),
        may_delete: may_write,
        allowed_file_types,
        attachment_limit_mb,
        me: Me {
            id: user.id,
            display_name: user.display_name,
            email: user.email,
            role: user.role,
        },
        today: OffsetDateTime::now_utc().date(),
    }))
}

/// Saves the title, the description and the deadline. Status is not here: the
/// column a task sits in is changed by moving it, which is the next slice.
/// Saves one region of the task, and leaves the rest of it alone.
///
/// Each field arrives only from the editor that was open, so a person fixing
/// a deadline cannot silently write back the description as it looked when
/// they opened the modal — the fields nobody edited are read from the store,
/// not from a hidden input carrying a stale copy.
#[server]
pub async fn save_task(
    task_id: String,
    title: Option<String>,
    description: Option<String>,
    deadline: Option<String>,
) -> Result<Option<Refusal>, ServerFnError> {
    use crate::server::accounts;
    use time::OffsetDateTime;
    use time::macros::format_description;

    let (user, facts) = match guard::writer_and_task(&task_id).await {
        Ok(pair) => pair,
        Err(refusal) => return Ok(Some(refusal)),
    };

    let title = match title {
        Some(given) => {
            let trimmed = given.trim().to_string();
            if trimmed.is_empty() {
                return Ok(Some(Refusal::EmptyTitle));
            }
            trimmed
        }
        None => facts.row.title.clone(),
    };
    let description = match &description {
        Some(given) => given.trim().to_string(),
        None => facts.description.clone(),
    };
    let deadline = match deadline.as_deref() {
        None => facts.row.deadline,
        Some(raw) => match raw.trim() {
            "" => None,
            day => match Date::parse(day, format_description!("[year]-[month]-[day]")) {
                Ok(day) => Some(day),
                Err(_) => return Ok(Some(Refusal::BadDeadline)),
            },
        },
    };

    accounts()
        .store()
        .save_task(
            &task_id,
            &title,
            &description,
            deadline,
            &user.id,
            OffsetDateTime::now_utc(),
        )
        .await
        .map_err(|e| ServerFnError::new(e.to_string()))?;
    Ok(None)
}

/// Puts someone on a task. A Viewer can neither do this nor be the target: a
/// viewer cannot be given work.
#[server]
pub async fn assign(task_id: String, user_id: String) -> Result<Option<Refusal>, ServerFnError> {
    use crate::server::accounts;
    use izlek_core::detail::ActivityKind;
    use time::OffsetDateTime;

    let (actor, _) = match guard::writer_and_task(&task_id).await {
        Ok(pair) => pair,
        Err(refusal) => return Ok(Some(refusal)),
    };
    let store = accounts().store().clone();
    let fail = |e: izlek_core::store::StoreError| ServerFnError::new(e.to_string());

    // The person named in the form is checked against this workspace, not
    // trusted, and a viewer is refused here rather than left out of the picker.
    let Some(person) = store.user(&user_id).await.map_err(fail)? else {
        return Ok(Some(Refusal::NotFound));
    };
    if person.workspace_id != actor.workspace_id {
        return Ok(Some(Refusal::NotFound));
    }
    if !person.role.can_be_assigned() {
        return Ok(Some(Refusal::Forbidden));
    }

    store
        .assign_task(&task_id, &person.id)
        .await
        .map_err(fail)?;
    store
        .record_activity(
            &task_id,
            Some(&actor.id),
            &ActivityKind::Assigned,
            &person.display_name,
            OffsetDateTime::now_utc(),
        )
        .await
        .map_err(fail)?;
    Ok(None)
}

#[server]
pub async fn unassign(task_id: String, user_id: String) -> Result<Option<Refusal>, ServerFnError> {
    use crate::server::accounts;
    use izlek_core::detail::ActivityKind;
    use time::OffsetDateTime;

    let (actor, _) = match guard::writer_and_task(&task_id).await {
        Ok(pair) => pair,
        Err(refusal) => return Ok(Some(refusal)),
    };
    let store = accounts().store().clone();
    let fail = |e: izlek_core::store::StoreError| ServerFnError::new(e.to_string());

    let Some(person) = store.user(&user_id).await.map_err(fail)? else {
        return Ok(Some(Refusal::NotFound));
    };
    if person.workspace_id != actor.workspace_id {
        return Ok(Some(Refusal::NotFound));
    }
    store
        .unassign_task(&task_id, &person.id)
        .await
        .map_err(fail)?;
    store
        .record_activity(
            &task_id,
            Some(&actor.id),
            &ActivityKind::Unassigned,
            &person.display_name,
            OffsetDateTime::now_utc(),
        )
        .await
        .map_err(fail)?;
    Ok(None)
}

/// Links two tasks. Both ends are checked against the asker's workspace, and a
/// link that would close a circle is refused by the store, inside the
/// transaction that would have written it.
#[server]
pub async fn link_tasks(
    task_id: String,
    other_id: String,
    direction: Direction,
) -> Result<Option<Refusal>, ServerFnError> {
    use crate::server::accounts;
    use izlek_core::detail::ActivityKind;
    use izlek_core::store::StoreError;
    use time::OffsetDateTime;

    let (actor, _) = match guard::writer_and_task(&task_id).await {
        Ok(pair) => pair,
        Err(refusal) => return Ok(Some(refusal)),
    };
    let store = accounts().store().clone();
    let other = match guard::task_of(store.as_ref(), &actor, &other_id).await {
        Ok(facts) => facts,
        Err(refusal) => return Ok(Some(refusal)),
    };

    let (blocked, blocking) = match direction {
        Direction::BlockedBy => (task_id.clone(), other_id.clone()),
        Direction::Blocks => (other_id.clone(), task_id.clone()),
    };
    let now = OffsetDateTime::now_utc();
    match store.add_dependency(&blocked, &blocking, now).await {
        Ok(()) => {}
        Err(StoreError::Cycle) => return Ok(Some(Refusal::Cycle)),
        Err(error) => return Err(ServerFnError::new(error.to_string())),
    }
    store
        .record_activity(
            &task_id,
            Some(&actor.id),
            &ActivityKind::Linked,
            &other.row.task_key,
            now,
        )
        .await
        .map_err(|e| ServerFnError::new(e.to_string()))?;
    Ok(None)
}

/// Clears a link. The row stays, marked cleared, so the history — and the
/// rules engine — still has something to read.
#[server]
pub async fn unlink_tasks(
    task_id: String,
    other_id: String,
    direction: Direction,
) -> Result<Option<Refusal>, ServerFnError> {
    use crate::server::accounts;
    use izlek_core::detail::ActivityKind;
    use time::OffsetDateTime;

    let (actor, _) = match guard::writer_and_task(&task_id).await {
        Ok(pair) => pair,
        Err(refusal) => return Ok(Some(refusal)),
    };
    let store = accounts().store().clone();
    let other = match guard::task_of(store.as_ref(), &actor, &other_id).await {
        Ok(facts) => facts,
        Err(refusal) => return Ok(Some(refusal)),
    };

    let (blocked, blocking) = match direction {
        Direction::BlockedBy => (task_id.clone(), other_id.clone()),
        Direction::Blocks => (other_id.clone(), task_id.clone()),
    };
    let now = OffsetDateTime::now_utc();
    let fail = |e: izlek_core::store::StoreError| ServerFnError::new(e.to_string());
    store
        .clear_dependency(&blocked, &blocking, now)
        .await
        .map_err(fail)?;
    store
        .record_activity(
            &task_id,
            Some(&actor.id),
            &ActivityKind::Unlinked,
            &other.row.task_key,
            now,
        )
        .await
        .map_err(fail)?;
    Ok(None)
}

/// Writes a comment. The author is the session's user; there is no author field
/// on the form. A Viewer is refused here, not merely shown no textarea.
#[server]
pub async fn post_comment(task_id: String, body: String) -> Result<Option<Refusal>, ServerFnError> {
    use crate::server::{accounts, require_user};
    use time::OffsetDateTime;

    let user = match require_user().await {
        Ok(user) => user,
        Err(refusal) => return Ok(Some(refusal)),
    };
    if !user.role.can_comment() {
        return Ok(Some(Refusal::Forbidden));
    }
    let store = accounts().store().clone();
    if let Err(refusal) = guard::task_of(store.as_ref(), &user, &task_id).await {
        return Ok(Some(refusal));
    }
    let body = body.trim();
    if body.is_empty() {
        return Ok(Some(Refusal::EmptyComment));
    }

    store
        .add_comment(&task_id, &user.id, body, OffsetDateTime::now_utc())
        .await
        .map_err(|e| ServerFnError::new(e.to_string()))?;
    Ok(None)
}

/// What a delete would take with it, for the confirmation step. Reads only.
#[server]
pub async fn what_delete_costs(
    task_id: String,
) -> Result<Result<DeletionCost, Refusal>, ServerFnError> {
    use crate::server::accounts;

    if let Err(refusal) = guard::writer_and_task(&task_id).await {
        return Ok(Err(refusal));
    }
    match accounts()
        .store()
        .deletion_cost(&task_id)
        .await
        .map_err(|e| ServerFnError::new(e.to_string()))?
    {
        Some(cost) => Ok(Ok(cost)),
        None => Ok(Err(Refusal::NotFound)),
    }
}

/// Deletes a task. A writer may: the delete is soft — the row keeps a
/// `deleted_at`, its comments and its edges stay in the table — so a mistake is
/// recoverable by hand. Whatever was waiting only on it becomes unblocked, the
/// store records that as an event, and the unblocked rules fire on it.
#[server]
pub async fn delete_task(task_id: String) -> Result<Option<Refusal>, ServerFnError> {
    use crate::server::accounts;
    use time::OffsetDateTime;

    let (user, _) = match guard::writer_and_task(&task_id).await {
        Ok(pair) => pair,
        Err(refusal) => return Ok(Some(refusal)),
    };
    let deletion = accounts()
        .store()
        .delete_task(&task_id, &user.id, OffsetDateTime::now_utc())
        .await
        .map_err(|e| ServerFnError::new(e.to_string()))?;
    // A blocker being deleted unblocks whatever was waiting only on it, which
    // is the same news as the blocker finishing. The freeing is committed; the
    // send is a separate step, off the request.
    if let Some(freeing) = deletion.event {
        crate::server::mail().after_freeing(freeing, deletion.freed);
    }
    Ok(None)
}

/// Deletes an attachment's row and its bytes. This is a hard delete, unlike
/// [`delete_task`]'s soft one: a file is a blob on disk, not a fact worth
/// keeping around for the audit trail, so there is nothing to undo by hand.
/// That is also why the gate is narrower than a writer's — only the person who
/// put the file there, or an admin cleaning up after them, may take it away.
#[server]
pub async fn delete_file(file_id: String) -> Result<Option<Refusal>, ServerFnError> {
    use crate::server::{accounts, require_user};

    let user = match require_user().await {
        Ok(user) => user,
        Err(refusal) => return Ok(Some(refusal)),
    };
    let store = accounts().store().clone();
    let Some(attachment) = store
        .attachment(&file_id)
        .await
        .map_err(|e| ServerFnError::new(e.to_string()))?
    else {
        return Ok(Some(Refusal::NotFound));
    };
    if let Err(refusal) = guard::task_of(store.as_ref(), &user, &attachment.task_id).await {
        return Ok(Some(refusal));
    }
    if user.id != attachment.uploaded_by && !user.role.can_administer() {
        return Ok(Some(Refusal::Forbidden));
    }

    store
        .delete_attachment(&file_id)
        .await
        .map_err(|e| ServerFnError::new(e.to_string()))?;
    Ok(None)
}

// -- the screen -------------------------------------------------------------

/// The artboard's STATUS control: the column the card sits in, and the way to
/// put it in another one.
///
/// It is a real `<form>` with a real `<select>`, so it works with wasm blocked
/// — that is what the submit button is for. When wasm is running, the change
/// event submits the form itself and the button is not drawn: picking a column
/// from a menu and then pressing a second control to confirm it is not what
/// the design draws, and not what anyone expects from a status field.
///
/// The column the card was in travels in a hidden field. It is the request's
/// claim about the board it was looking at, and the store refuses the move if
/// that claim has gone stale.
#[component]
fn StatusControl(
    task_id: StoredValue<String>,
    column_id: String,
    columns: Vec<izlek_core::board::Column>,
    action: ServerAction<crate::board::MoveCard>,
) -> impl IntoView {
    // An effect never runs on the server, so this is false through SSR and
    // true once — and only if — hydration happens. It is the honest answer to
    // "is script running here", which is the question the button depends on.
    let live = RwSignal::new(false);
    Effect::new(move |_| live.set(true));

    let here = column_id.clone();
    let done_column = columns
        .iter()
        .find(|column| column.id == column_id)
        .map(|column| column.is_done)
        .unwrap_or(false);

    view! {
        <ActionForm action=action attr:class="status-form">
            <input type="hidden" name="task_id" value=move || task_id.get_value()/>
            <input type="hidden" name="from_column_id" value=column_id.clone()/>
            <span class="status-dot" class:status-dot-done=done_column></span>
            <select
                class="status-select"
                name="to_column_id"
                on:change=move |event| {
                    let _ = &event;
                    #[cfg(feature = "hydrate")]
                    {
                        use leptos::wasm_bindgen::JsCast;
                        if let Some(select) = event
                            .target()
                            .and_then(|target| {
                                target.dyn_into::<leptos::web_sys::HtmlSelectElement>().ok()
                            })
                        {
                            if let Some(form) = select.form() {
                                let _ = form.request_submit();
                            }
                        }
                    }
                }
            >
                {columns
                    .iter()
                    .map(|column| {
                        let selected = column.id == here;
                        view! {
                            <option value=column.id.clone() selected=selected>
                                {column.name.clone()}
                            </option>
                        }
                    })
                    .collect_view()}
            </select>
            {glyph::chevron()}
            <Show when=move || !live.get()>
                <button class="status-go" type="submit">
                    "Move"
                </button>
            </Show>
        </ActionForm>
    }
}

/// Whatever a refused call said, rendered where the form that asked for it is.
/// A refusal shown at the far end of a scrolling modal is a refusal nobody
/// reads: the person clicked "Link a task" and the sentence has to land under
/// that button, not under the activity trail.
fn refused(refusal: impl Fn() -> Option<Refusal> + Copy + Send + Sync + 'static) -> impl IntoView {
    move || refusal().map(|refusal| view! { <p class="modal-problem">{refusal.message()}</p> })
}

/// The modal. `esc` closes it, as the artboard's chip says.
#[component]
pub fn TaskDetailModal(
    task_id: String,
    on_close: impl Fn() + Copy + Send + Sync + 'static,
    on_change: impl Fn() + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let task_id = StoredValue::new(task_id);
    let detail = Resource::new(
        move || task_id.get_value(),
        |id| async move { fetch_task(id).await },
    );
    // Every change writes and then re-reads: the modal shows what the store
    // says, not what the browser hoped.
    let changed = move || {
        detail.refetch();
        on_change();
    };

    // Escape listens on the window, not on the modal. The modal is inserted
    // after the click that opened it, so focus is still on the card behind the
    // scrim and a listener bound to this element would never hear the key.
    #[cfg(feature = "hydrate")]
    {
        let handle = window_event_listener(leptos::ev::keydown, move |event| {
            if event.key() == "Escape" {
                on_close();
            }
        });
        on_cleanup(move || handle.remove());
    }

    view! {
        <div class="modal-scrim" on:click=move |_| on_close()>
            <div class="modal" tabindex="-1" on:click=move |event| event.stop_propagation()>
                <Suspense fallback=|| {
                    view! { <div class="modal-loading"></div> }
                }>
                    {move || Suspend::new(async move {
                        match detail.await {
                            Ok(Ok(snapshot)) => {
                                view! {
                                    <DetailScreen
                                        snapshot=snapshot
                                        on_close=on_close
                                        on_change=changed
                                    />
                                }
                                    .into_any()
                            }
                            Ok(Err(refusal)) => {
                                view! { <p class="modal-note">{refusal.message()}</p> }.into_any()
                            }
                            Err(_) => {
                                view! {
                                    <p class="modal-note">"Something went wrong."</p>
                                }
                                    .into_any()
                            }
                        }
                    })}
                </Suspense>
            </div>
        </div>
    }
}

/// The artboard's glyphs, drawn rather than typed. A missing character in a
/// font is a hole in the design; an inline path is the same shape everywhere.
mod glyph {
    use leptos::prelude::*;

    pub fn chevron() -> impl IntoView {
        view! {
            <svg
                class="glyph"
                width="14"
                height="14"
                viewBox="0 0 16 16"
                fill="none"
                stroke="currentColor"
                stroke-width="1.5"
                stroke-linecap="round"
                stroke-linejoin="round"
                aria-hidden="true"
            >
                <path d="M4 6l4 4 4-4"></path>
            </svg>
        }
    }

    pub fn calendar() -> impl IntoView {
        view! {
            <svg
                class="glyph"
                width="13"
                height="13"
                viewBox="0 0 16 16"
                fill="none"
                stroke="currentColor"
                stroke-width="1.5"
                stroke-linecap="round"
                aria-hidden="true"
            >
                <rect x="2.5" y="3.5" width="11" height="10" rx="1.5"></rect>
                <path d="M2.5 6.5h11M5.5 2v2.5M10.5 2v2.5"></path>
            </svg>
        }
    }

    pub fn plus() -> impl IntoView {
        view! {
            <svg
                class="glyph"
                width="12"
                height="12"
                viewBox="0 0 16 16"
                fill="none"
                stroke="currentColor"
                stroke-width="1.6"
                stroke-linecap="round"
                aria-hidden="true"
            >
                <path d="M8 3v10M3 8h10"></path>
            </svg>
        }
    }

    pub fn cross() -> impl IntoView {
        view! {
            <svg
                class="glyph"
                width="13"
                height="13"
                viewBox="0 0 16 16"
                fill="none"
                stroke="currentColor"
                stroke-width="1.5"
                stroke-linecap="round"
                aria-hidden="true"
            >
                <path d="M4 4l8 8M12 4l-8 8"></path>
            </svg>
        }
    }

    pub fn tick() -> impl IntoView {
        view! {
            <svg
                class="glyph dep-tick"
                width="14"
                height="14"
                viewBox="0 0 16 16"
                fill="none"
                stroke="currentColor"
                stroke-width="1.8"
                stroke-linecap="round"
                stroke-linejoin="round"
                aria-hidden="true"
            >
                <path d="M3 8.5l3.5 3.5L13 5"></path>
            </svg>
        }
    }

    pub fn lock() -> impl IntoView {
        view! {
            <svg
                class="glyph"
                width="14"
                height="14"
                viewBox="0 0 16 16"
                fill="none"
                stroke="currentColor"
                stroke-width="1.6"
                stroke-linecap="round"
                stroke-linejoin="round"
                aria-hidden="true"
            >
                <rect x="3.5" y="7" width="9" height="6.5" rx="1.5"></rect>
                <path d="M5.5 7V5a2.5 2.5 0 015 0v2"></path>
            </svg>
        }
    }

    pub fn bin() -> impl IntoView {
        view! {
            <svg
                class="glyph"
                width="14"
                height="14"
                viewBox="0 0 16 16"
                fill="none"
                stroke="currentColor"
                stroke-width="1.5"
                stroke-linecap="round"
                stroke-linejoin="round"
                aria-hidden="true"
            >
                <path d="M3 4.5h10M6.5 4.5V3h3v1.5M4.5 4.5l0.7 8.5a1 1 0 001 0.9h3.6a1 1 0 001-0.9l0.7-8.5"></path>
            </svg>
        }
    }
}

/// The title, as the artboard draws it: the heading itself, with the rename
/// hidden behind it.
///
/// Editing is a state the screen enters, not a second field sitting beside the
/// first. The checkbox is the state — it is what a click on the heading flips,
/// what Cancel flips back, and it needs no script to do either. The heading and
/// the input are never on screen together.
#[component]
fn TitleControl(
    task_id: StoredValue<String>,
    title: String,
    may_write: bool,
    on_change: impl Fn() + Copy + Send + Sync + 'static,
) -> impl IntoView {
    if !may_write {
        return Either::Left(view! { <h2 class="detail-title">{title}</h2> });
    }
    let action = ServerAction::<SaveTask>::new();
    let refusal = crate::auth::refusal_of(action);
    let value = action.value();
    Effect::new(move |_| {
        if matches!(value.get(), Some(Ok(None))) {
            on_change();
        }
    });
    let toggle = format!("rename-{}", task_id.get_value());
    let field = title.clone();

    Either::Right(view! {
        <div class="edit">
            <input
                class="edit-toggle"
                // Open, if the page was landed on carrying this call's refusal.
                // Without script the sentence is rendered inside this region,
                // and a region that is closed says nothing at all.
                checked=move || refusal().is_some()
                type="checkbox"
                id=toggle.clone()
                aria-label="Rename this task"
            />
            <h2 class="detail-title edit-view">
                <label class="edit-hit" for=toggle.clone()>
                    {title}
                </label>
            </h2>
            <ActionForm action=action attr:class="edit-form title-form">
                <input type="hidden" name="task_id" value=move || task_id.get_value()/>
                <input
                    class="title-input"
                    type="text"
                    name="title"
                    value=field
                    autocomplete="off"
                    required
                />
                <button class="edit-save" type="submit" disabled=move || action.pending().get()>
                    "Save"
                </button>
                <label class="edit-cancel" for=toggle>
                    "Cancel"
                </label>
            </ActionForm>
            {refused(refusal)}
        </div>
    })
}

/// The description as prose, with the textarea behind it.
///
/// Clicking the prose swaps this region and only this region. Nothing else on
/// the screen moves, and the rest of the task is not carried along in hidden
/// inputs — the save names the description alone.
#[component]
fn DescriptionControl(
    task_id: StoredValue<String>,
    description: String,
    may_write: bool,
    on_change: impl Fn() + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let empty = description.trim().is_empty();
    let prose = if empty {
        "No description yet.".to_string()
    } else {
        description.clone()
    };

    if !may_write {
        return Either::Left(
            view! { <p class="detail-prose" class:detail-prose-empty=empty>{prose}</p> },
        );
    }
    let action = ServerAction::<SaveTask>::new();
    let refusal = crate::auth::refusal_of(action);
    let value = action.value();
    Effect::new(move |_| {
        if matches!(value.get(), Some(Ok(None))) {
            on_change();
        }
    });
    let toggle = format!("describe-{}", task_id.get_value());
    let field = description.clone();

    Either::Right(view! {
        <div class="edit">
            <input
                class="edit-toggle"
                // Open, if the page was landed on carrying this call's refusal.
                // Without script the sentence is rendered inside this region,
                // and a region that is closed says nothing at all.
                checked=move || refusal().is_some()
                type="checkbox"
                id=toggle.clone()
                aria-label="Edit the description"
            />
            <label class="detail-prose edit-view edit-hit" class:detail-prose-empty=empty for=toggle.clone()>
                {prose}
            </label>
            <ActionForm action=action attr:class="edit-form describe-form">
                <input type="hidden" name="task_id" value=move || task_id.get_value()/>
                <textarea class="detail-textarea" name="description" rows="5" prop:value=field.clone()>
                    {field.clone()}
                </textarea>
                <div class="edit-row">
                    <button class="edit-save" type="submit" disabled=move || action.pending().get()>
                        "Save"
                    </button>
                    <label class="edit-cancel" for=toggle>
                        "Cancel"
                    </label>
                </div>
            </ActionForm>
            {refused(refusal)}
        </div>
    })
}

/// One deadline control: the artboard's calendar glyph, the date as a person
/// reads it, and the chevron. The picker is what opens when it is clicked —
/// there is no second DEADLINE row further down the screen.
#[component]
fn DeadlineControl(
    task_id: StoredValue<String>,
    deadline_label: String,
    deadline_input: String,
    overdue: bool,
    may_write: bool,
    on_change: impl Fn() + Copy + Send + Sync + 'static,
) -> impl IntoView {
    if !may_write {
        return Either::Left(view! {
            <span class="field-box" class:detail-overdue=overdue>
                {glyph::calendar()}
                <span class="field-text">{deadline_label}</span>
            </span>
        });
    }
    let action = ServerAction::<SaveTask>::new();
    let refusal = crate::auth::refusal_of(action);
    let value = action.value();
    Effect::new(move |_| {
        if matches!(value.get(), Some(Ok(None))) {
            on_change();
        }
    });
    let toggle = format!("deadline-{}", task_id.get_value());

    Either::Right(view! {
        <div class="edit edit-pop">
            <input
                class="edit-toggle"
                // Open, if the page was landed on carrying this call's refusal.
                // Without script the sentence is rendered inside this region,
                // and a region that is closed says nothing at all.
                checked=move || refusal().is_some()
                type="checkbox"
                id=toggle.clone()
                aria-label="Change the deadline"
            />
            <label
                class="field-box edit-view edit-hit"
                class:detail-overdue=overdue
                for=toggle.clone()
            >
                {glyph::calendar()}
                <span class="field-text">{deadline_label}</span>
                {glyph::chevron()}
            </label>
            <div class="edit-form pop-panel">
                <ActionForm action=action attr:class="pop-form">
                    <input type="hidden" name="task_id" value=move || task_id.get_value()/>
                    <input class="detail-date" type="date" name="deadline" value=deadline_input/>
                    <div class="edit-row">
                        <button
                            class="edit-save"
                            type="submit"
                            disabled=move || action.pending().get()
                        >
                            "Save"
                        </button>
                        <label class="edit-cancel" for=toggle>
                            "Cancel"
                        </label>
                    </div>
                </ActionForm>
                {refused(refusal)}
            </div>
        </div>
    })
}
/// The round "+" from the artboard, and the list of people it opens.
///
/// A native `<select>` cannot be made to look like this control, so the picker
/// is the same popover the deadline uses: a checkbox holds the open state, and
/// each person is their own one-field form. It works with the wasm bundle
/// blocked, and it is still the design's round button.
#[component]
fn AssigneePicker(
    task_id: StoredValue<String>,
    people: Vec<Person>,
    on_change: impl Fn() + Copy + Send + Sync + 'static,
) -> impl IntoView {
    if people.is_empty() {
        return None;
    }
    let action = ServerAction::<Assign>::new();
    let refusal = crate::auth::refusal_of(action);
    let value = action.value();
    Effect::new(move |_| {
        if matches!(value.get(), Some(Ok(None))) {
            on_change();
        }
    });
    let toggle = format!("assign-{}", task_id.get_value());

    Some(view! {
        <div class="edit edit-pop assignee-pop">
            <input
                class="edit-toggle"
                // Open, if the page was landed on carrying this call's refusal.
                // Without script the sentence is rendered inside this region,
                // and a region that is closed says nothing at all.
                checked=move || refusal().is_some()
                type="checkbox"
                id=toggle.clone()
                aria-label="Put someone on this task"
            />
            <label class="assignee-add edit-view edit-hit" for=toggle.clone()>
                {glyph::plus()}
            </label>
            <div class="edit-form pop-panel">
                <div class="pop-list">
                    {people
                        .into_iter()
                        .map(|person| {
                            let name = person.display_name.clone();
                            view! {
                                <ActionForm action=action attr:class="pop-row-form">
                                    <input
                                        type="hidden"
                                        name="task_id"
                                        value=move || task_id.get_value()
                                    />
                                    <input type="hidden" name="user_id" value=person.id.clone()/>
                                    <button class="pop-row" type="submit">
                                        <Avatar person=person.clone() extra="avatar-sm"/>
                                        <span class="pop-row-name">{name}</span>
                                    </button>
                                </ActionForm>
                            }
                        })
                        .collect_view()}
                </div>
                {refused(refusal)}
            </div>
        </div>
    })
}

/// The 26px "Link a task" chip in the DEPENDENCIES header, and the picker it
/// opens: the tasks as a list, the direction as two words, one button.
#[component]
fn LinkPicker(
    task_id: StoredValue<String>,
    linkable: Vec<LinkTarget>,
    on_change: impl Fn() + Copy + Send + Sync + 'static,
) -> impl IntoView {
    if linkable.is_empty() {
        return None;
    }
    let action = ServerAction::<LinkTasks>::new();
    let refusal = crate::auth::refusal_of(action);
    let value = action.value();
    Effect::new(move |_| {
        if matches!(value.get(), Some(Ok(None))) {
            on_change();
        }
    });
    let toggle = format!("link-{}", task_id.get_value());
    let direction = format!("link-direction-{}", task_id.get_value());

    Some(view! {
        <div class="edit edit-pop link-pop">
            <input
                class="edit-toggle"
                // Open, if the page was landed on carrying this call's refusal.
                // Without script the sentence is rendered inside this region,
                // and a region that is closed says nothing at all.
                checked=move || refusal().is_some()
                type="checkbox"
                id=toggle.clone()
                aria-label="Link another task"
            />
            <label class="dep-chip edit-view edit-hit" for=toggle.clone()>
                {glyph::plus()}
                <span class="dep-chip-text">"Link a task"</span>
            </label>
            <div class="edit-form pop-panel pop-panel-wide">
                <ActionForm action=action attr:class="pop-form">
                    <input type="hidden" name="task_id" value=move || task_id.get_value()/>
                    <div class="pop-list pop-list-scroll">
                        {linkable
                            .iter()
                            .map(|target| {
                                let key = target.task_key.clone();
                                let title = target.title.clone();
                                view! {
                                    <label class="pick-row">
                                        <input
                                            type="radio"
                                            name="other_id"
                                            value=target.id.clone()
                                            required
                                        />
                                        <span class="dep-key">{key}</span>
                                        <span class="pick-title">{title}</span>
                                    </label>
                                }
                            })
                            .collect_view()}
                    </div>
                    <fieldset class="pick-direction">
                        <legend class="detail-label">"DIRECTION"</legend>
                        <label class="pick-row">
                            <input
                                type="radio"
                                name="direction"
                                value="blocked_by"
                                checked
                                id=format!("{direction}-blocked-by")
                            />
                            <span class="pick-title">"blocks this task"</span>
                        </label>
                        <label class="pick-row">
                            <input
                                type="radio"
                                name="direction"
                                value="blocks"
                                id=format!("{direction}-blocks")
                            />
                            <span class="pick-title">"waits on this task"</span>
                        </label>
                    </fieldset>
                    <div class="edit-row">
                        <button
                            class="edit-save"
                            type="submit"
                            disabled=move || action.pending().get()
                        >
                            "Link"
                        </button>
                        <label class="edit-cancel" for=toggle>
                            "Cancel"
                        </label>
                    </div>
                </ActionForm>
                {refused(refusal)}
            </div>
        </div>
    })
}
#[component]
fn DetailScreen(
    snapshot: DetailSnapshot,
    on_close: impl Fn() + Copy + Send + Sync + 'static,
    on_change: impl Fn() + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let DetailSnapshot {
        detail,
        me,
        today,
        linkable,
        may_write,
        may_comment,
        may_delete,
        allowed_file_types,
        attachment_limit_mb: _,
    } = snapshot;

    let id = StoredValue::new(detail.id.clone());
    let task_key = detail.task_key.clone();
    let title = detail.title.clone();
    let description = detail.description.clone();
    let status = detail.column.name.clone();
    let column_id = detail.column.id.clone();
    let columns = detail.columns.clone();
    let deadline_input = detail.deadline_input();
    let overdue = detail.is_overdue(today);
    let deadline_label = detail.deadline_label(today);
    let assignee_count = detail.assignees.len();
    let comment_count = detail.comments.len();
    let file_count = detail.files.len();
    let unassigned: Vec<Person> = detail.unassigned().cloned().collect();
    let has_deps = !detail.blocked_by.is_empty() || !detail.blocks.is_empty();
    let accept = (!allowed_file_types.is_empty()).then(|| {
        allowed_file_types
            .iter()
            .map(|kind| format!(".{kind}"))
            .collect::<Vec<_>>()
            .join(",")
    });
    let upload_refusal = crate::auth::refusal_from_query("upload_file");

    let comment = ServerAction::<PostComment>::new();
    let remove = ServerAction::<DeleteTask>::new();
    let move_to = ServerAction::<crate::board::MoveCard>::new();
    let drop_file = ServerAction::<DeleteFile>::new();
    let comment_refusal = crate::auth::refusal_of(comment);
    let remove_refusal = crate::auth::refusal_of(remove);
    let move_refusal = crate::auth::refusal_of(move_to);
    let drop_file_refusal = crate::auth::refusal_of(drop_file);
    // Deleting is two steps: ask what it would cost, then say it out loud and
    // let the person decide. The artboard's red button had no confirmation and
    // this action reaches other people's tasks.
    let ask = ServerAction::<WhatDeleteCosts>::new();
    // Every action that lands re-reads the task and the board behind it. The
    // per-region editors own their own actions and do the same.
    for value in [comment.value(), move_to.value(), drop_file.value()] {
        Effect::new(move |_| {
            if matches!(value.get(), Some(Ok(None))) {
                on_change();
            }
        });
    }
    let removed = remove.value();
    Effect::new(move |_| {
        if matches!(removed.get(), Some(Ok(None))) {
            on_change();
            on_close();
        }
    });

    view! {
        <header class="detail-head">
            <div class="detail-headline">
                <span class="detail-key">{task_key}</span>
                <TitleControl task_id=id title=title may_write=may_write on_change=on_change/>
            </div>
            <span class="detail-esc">"esc"</span>
            // A link back to the bare board, so closing works without script; with
            // script the handler closes in place and the navigation is cancelled.
            <a
                class="detail-close"
                href="/"
                aria-label="Close this task"
                on:click=move |event| {
                    event.prevent_default();
                    on_close();
                }
            >
                {glyph::cross()}
            </a>
        </header>

        <div class="detail-fields">
            <div class="detail-field">
                <span class="detail-label">"STATUS"</span>
                {if may_write {
                    Either::Left(
                        view! {
                            <StatusControl
                                task_id=id
                                column_id=column_id
                                columns=columns
                                action=move_to
                            />
                        },
                    )
                } else {
                    Either::Right(
                        view! {
                            <span class="field-box">
                                <span class="status-dot"></span>
                                <span class="field-text">{status}</span>
                            </span>
                        },
                    )
                }}
                {refused(move_refusal)}
            </div>
            <div class="detail-field">
                <span class="detail-label">
                    {format!("ASSIGNEES — {assignee_count}")}
                </span>
                <div class="detail-assignees">
                    {detail
                        .assignees
                        .iter()
                        .map(|person| {
                            view! {
                                <AssigneeChip
                                    task_id=id
                                    person=person.clone()
                                    may_write=may_write
                                    on_change=on_change
                                />
                            }
                        })
                        .collect_view()}
                    <div class="spacer"></div>
                    {may_write
                        .then(|| {
                            view! { <AssigneePicker task_id=id people=unassigned on_change=on_change/> }
                        })}
                </div>
            </div>
            <div class="detail-field">
                <span class="detail-label">"DEADLINE"</span>
                <DeadlineControl
                    task_id=id
                    deadline_label=deadline_label
                    deadline_input=deadline_input
                    overdue=overdue
                    may_write=may_write
                    on_change=on_change
                />
            </div>
        </div>

        <section class="detail-block">
            <span class="detail-label">"DESCRIPTION"</span>
            <DescriptionControl
                task_id=id
                description=description
                may_write=may_write
                on_change=on_change
            />
        </section>

        <section class="detail-block">
            <div class="detail-block-head">
                <span class="detail-label">"DEPENDENCIES"</span>
                <div class="spacer"></div>
                {may_write
                    .then(|| {
                        view! { <LinkPicker task_id=id linkable=linkable on_change=on_change/> }
                    })}
            </div>
            {has_deps
                .then(|| {
                    view! {
                        <div class="dep-list">
                            {detail
                                .blocked_by
                                .iter()
                                .map(|edge| {
                                    view! {
                                        <DepRow
                                            task_id=id
                                            edge=edge.clone()
                                            direction=Direction::BlockedBy
                                            may_write=may_write
                                            on_change=on_change
                                        />
                                    }
                                })
                                .collect_view()}
                            {detail
                                .blocks
                                .iter()
                                .map(|edge| {
                                    view! {
                                        <DepRow
                                            task_id=id
                                            edge=edge.clone()
                                            direction=Direction::Blocks
                                            may_write=may_write
                                            on_change=on_change
                                        />
                                    }
                                })
                                .collect_view()}
                        </div>
                    }
                })}
            {(!has_deps).then(|| view! { <p class="detail-quiet">"Nothing blocks this task."</p> })}
        </section>

        <section class="detail-block">
            <div class="detail-block-head">
                <span class="detail-label">"FILES"</span>
                <span class="detail-count">{file_count}</span>
            </div>
            {(file_count > 0)
                .then(|| {
                    view! {
                        <div class="file-list">
                            {detail
                                .files
                                .iter()
                                .map(|file| {
                                    view! { <FileChip file=file.clone() me=me.clone() action=drop_file/> }
                                })
                                .collect_view()}
                        </div>
                    }
                })}
            {(file_count == 0).then(|| view! { <p class="detail-quiet">"No files yet."</p> })}
            {refused(drop_file_refusal)}
            {may_comment
                .then(|| {
                    view! {
                        <form
                            class="file-upload"
                            method="post"
                            action="/files"
                            enctype="multipart/form-data"
                        >
                            <input type="hidden" name="task_id" value=move || id.get_value()/>
                            <input type="file" name="file" accept=accept required/>
                            <button class="file-upload-submit" type="submit">
                                "Upload"
                            </button>
                        </form>
                        {refused(upload_refusal)}
                    }
                })}
        </section>

        <section class="detail-block">
            <div class="detail-block-head">
                <span class="detail-label">"COMMENTS"</span>
                <span class="detail-count">{comment_count}</span>
            </div>
            <div class="comment-list">
                {detail
                    .comments
                    .iter()
                    .map(|entry| view! { <CommentRow comment=entry.clone()/> })
                    .collect_view()}
                {may_comment
                    .then(|| {
                        view! {
                            <ActionForm action=comment attr:class="comment-composer">
                                <input type="hidden" name="task_id" value=move || id.get_value()/>
                                <textarea
                                    class="comment-input"
                                    name="body"
                                    rows="3"
                                    placeholder="Write a comment…"
                                    required
                                ></textarea>
                                <div class="comment-row">
                                    <span class="comment-hint">"⌘↵ to post"</span>
                                    <div class="spacer"></div>
                                    <button
                                        class="comment-post"
                                        type="submit"
                                        disabled=move || comment.pending().get()
                                    >
                                        "Comment"
                                    </button>
                                </div>
                            </ActionForm>
                            {refused(comment_refusal)}
                        }
                    })}
            </div>
        </section>

        <section class="detail-block">
            <span class="detail-label">"ACTIVITY"</span>
            {detail
                .activity
                .iter()
                .map(|entry| {
                    let who = entry
                        .actor
                        .as_ref()
                        .map(|person| person.display_name.clone())
                        .unwrap_or_else(|| "Izlek".to_string());
                    view! {
                        <div class="activity-line">
                            <span class="activity-stamp">{entry.moment()}</span>
                            <strong class="activity-who">{who}</strong>
                            <span class="activity-what">{entry.sentence()}</span>
                        </div>
                    }
                })
                .collect_view()}
        </section>

        {refused(remove_refusal)}

        {move || {
            ask.value()
                .get()
                .and_then(|answer| answer.ok().and_then(|inner| inner.ok()))
                .map(|cost| {
                    let DeletionCost { task_key, title, comment_count, link_count, frees } = cost;
                    let freed = frees.join(", ");
                    view! {
                        <div class="confirm">
                            <div class="confirm-title">
                                {format!("Delete {task_key} — {title}?")}
                            </div>
                            <ul class="confirm-list">
                                {(comment_count > 0)
                                    .then(|| {
                                        view! {
                                            <li>
                                                {if comment_count == 1 {
                                                    "1 comment goes with it".to_string()
                                                } else {
                                                    format!("{comment_count} comments go with it")
                                                }}
                                            </li>
                                        }
                                    })}
                                {(link_count > 0)
                                    .then(|| {
                                        view! {
                                            <li>
                                                {if link_count == 1 {
                                                    "1 dependency stops applying".to_string()
                                                } else {
                                                    format!("{link_count} dependencies stop applying")
                                                }}
                                            </li>
                                        }
                                    })}
                                {(!freed.is_empty())
                                    .then(|| {
                                        view! { <li>{format!("{freed} stops being blocked")}</li> }
                                    })}
                            </ul>
                            <div class="confirm-note">
                                "The task keeps its record in the database, but nothing in Izlek brings it back."
                            </div>
                            <div class="confirm-row">
                                <button
                                    class="detail-cancel"
                                    type="button"
                                    on:click=move |_| ask.value().set(None)
                                >
                                    "Keep it"
                                </button>
                                <ActionForm action=remove attr:class="detail-delete-form">
                                    <input type="hidden" name="task_id" value=move || id.get_value()/>
                                    <button
                                        class="detail-delete detail-delete-sure"
                                        type="submit"
                                        disabled=move || remove.pending().get()
                                    >
                                        {format!("Delete {task_key}")}
                                    </button>
                                </ActionForm>
                            </div>
                        </div>
                    }
                })
        }}

        <footer class="detail-foot">
            {may_delete
                .then(|| {
                    view! {
                        <ActionForm action=ask attr:class="detail-delete-form">
                            <input type="hidden" name="task_id" value=move || id.get_value()/>
                            <button
                                class="detail-delete"
                                type="submit"
                                disabled=move || ask.pending().get()
                            >
                                {glyph::bin()}
                                <span>"Delete task"</span>
                            </button>
                        </ActionForm>
                    }
                })}
            <div class="spacer"></div>
            <button class="detail-cancel" type="button" on:click=move |_| on_close()>
                "Close"
            </button>
        </footer>
    }
}

#[component]
fn AssigneeChip(
    task_id: StoredValue<String>,
    person: Person,
    may_write: bool,
    on_change: impl Fn() + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let action = ServerAction::<Unassign>::new();
    let value = action.value();
    Effect::new(move |_| {
        if matches!(value.get(), Some(Ok(None))) {
            on_change();
        }
    });
    let name = person.display_name.clone();
    let person_id = person.id.clone();
    // A chip carries a first name or no name at all. Half a name with an
    // ellipsis after it — "Be…" — is noise where a name should be, and an
    // avatar on its own still says who it is.
    let full_name = name.clone();
    let first_name = name
        .split_whitespace()
        .next()
        .unwrap_or(name.as_str())
        .to_owned();

    view! {
        <span class="assignee-chip" title=full_name>
            <Avatar person=person extra="avatar-sm"/>
            <span class="assignee-name">{first_name}</span>
            {may_write
                .then(|| {
                    view! {
                        <ActionForm action=action attr:class="assignee-drop">
                            <input type="hidden" name="task_id" value=move || task_id.get_value()/>
                            <input type="hidden" name="user_id" value=person_id/>
                            <button
                                class="assignee-remove"
                                type="submit"
                                title=format!("Take {name} off this task")
                            >
                                {glyph::cross()}
                            </button>
                        </ActionForm>
                    }
                })}
        </span>
    }
}

#[component]
fn DepRow(
    task_id: StoredValue<String>,
    edge: DependencyEdge,
    direction: Direction,
    may_write: bool,
    on_change: impl Fn() + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let action = ServerAction::<UnlinkTasks>::new();
    let value = action.value();
    Effect::new(move |_| {
        if matches!(value.get(), Some(Ok(None))) {
            on_change();
        }
    });
    let cleared = edge.is_cleared();
    let note = match direction {
        Direction::BlockedBy => edge.blocked_by_label(),
        Direction::Blocks => edge.blocks_label(),
    };
    // Amber means someone is stuck. This task blocking another one is the
    // normal state of a dependency and says nothing about this task, so the
    // colour belongs to the row on the receiving end — and only while its
    // blocker is unfinished. Otherwise the board's "1 blocked" pill and the row
    // colour tell two different stories about who is waiting.
    let waiting = matches!(direction, Direction::BlockedBy) && !cleared;
    let other_id = edge.task_id.clone();
    let wire = match direction {
        Direction::BlockedBy => "blocked_by",
        Direction::Blocks => "blocks",
    };
    // The artboard puts the direction in the row itself, in a fixed column, so
    // the rows line up whichever way round they run.
    let tag = match direction {
        Direction::BlockedBy => "BLOCKED BY",
        Direction::Blocks => "BLOCKS",
    };

    view! {
        <div class="dep-row" class:dep-row-waiting=waiting>
            <span class="dep-tag">{tag}</span>
            {if cleared {
                Either::Left(glyph::tick())
            } else {
                Either::Right(glyph::lock())
            }}
            <span class="dep-key">{edge.task_key.clone()}</span>
            <span class="dep-title">{edge.title.clone()}</span>
            <div class="spacer"></div>
            <span class="dep-note">{note}</span>
            {may_write
                .then(|| {
                    view! {
                        <ActionForm action=action attr:class="dep-unlink-form">
                            <input type="hidden" name="task_id" value=move || task_id.get_value()/>
                            <input type="hidden" name="other_id" value=other_id/>
                            <input type="hidden" name="direction" value=wire/>
                            <button class="dep-unlink" type="submit" title="Remove this link">
                                {glyph::cross()}
                            </button>
                        </ActionForm>
                    }
                })}
        </div>
    }
}

/// One attachment, chip-shaped like a dependency. A file posted with a
/// comment stays listed here too — a person hunting for what someone
/// attached should not have to remember which comment it rode in on — with a
/// quiet note saying where it came from.
#[component]
fn FileChip(
    file: izlek_core::detail::FileLine,
    me: Me,
    action: ServerAction<DeleteFile>,
) -> impl IntoView {
    let may_drop = me.id == file.uploaded_by || me.role.can_administer();
    let size = file.size_label();
    let on_comment = file.comment_id.is_some();
    let file_id = file.id.clone();

    view! {
        <span class="file-chip">
            <a class="file-chip-name" href=format!("/files/{}", file.id)>
                {file.name.clone()}
            </a>
            <span class="file-chip-size">{size}</span>
            {on_comment.then(|| view! { <span class="file-chip-note">"on a comment"</span> })}
            {may_drop
                .then(|| {
                    view! {
                        <ActionForm action=action attr:class="file-chip-drop-form">
                            <input type="hidden" name="file_id" value=file_id/>
                            <button class="file-chip-drop" type="submit" title="Remove this file">
                                {glyph::cross()}
                            </button>
                        </ActionForm>
                    }
                })}
        </span>
    }
}

#[component]
fn CommentRow(comment: Comment) -> impl IntoView {
    use izlek_core::detail::moment_label;

    let author = comment.author.clone();
    view! {
        <div class="comment">
            <Avatar person=author extra="avatar-lg"/>
            <div class="comment-said">
                <div class="comment-head">
                    <span class="comment-who">{comment.author.display_name.clone()}</span>
                    <span class="comment-when">{moment_label(comment.at)}</span>
                </div>
                <div class="comment-body">{comment.body.clone()}</div>
            </div>
        </div>
    }
}
