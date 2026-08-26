//! The task detail modal, from the TaskDetail artboard.
//!
//! One server call brings the whole screen: the task, its assignees, both
//! directions of its dependencies, its comments and its activity. Every change
//! is its own call, and every one of them checks the task belongs to the
//! asker's workspace before it does anything — a task id in a form is an
//! authorization question, not a validation one.

use dizey_core::board::Person;
use dizey_core::detail::{Comment, DeletionCost, DependencyEdge, TaskDetail};
use leptos::prelude::*;
use serde::{Deserialize, Serialize};
use time::Date;

use crate::auth::{Me, Refusal};

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
mod guard {
    use crate::auth::Refusal;
    use dizey_core::detail::TaskFacts;
    use dizey_core::store::User;

    /// The task, if this person's workspace is the one holding it. A task in
    /// another workspace is not found rather than forbidden: the answer says
    /// nothing about whether the id is real.
    pub async fn task_of(user: &User, task_id: &str) -> Result<TaskFacts, Refusal> {
        let store = crate::server::accounts().store().clone();
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
        let facts = task_of(&user, task_id).await?;
        Ok((user, facts))
    }
}

/// Everything one task detail shows. A Viewer may read it; what they may not do
/// is refused in the calls below, not merely hidden from them here.
#[server]
pub async fn fetch_task(task_id: String) -> Result<Result<DetailSnapshot, Refusal>, ServerFnError> {
    use crate::server::{accounts, require_user};
    use dizey_core::detail::load;
    use time::OffsetDateTime;

    let user = match require_user().await {
        Ok(user) => user,
        Err(refusal) => return Ok(Err(refusal)),
    };
    let store = accounts().store().clone();
    let fail = |e: dizey_core::store::StoreError| ServerFnError::new(e.to_string());

    let Some(detail) = load(store.as_ref(), &user.workspace_id, &task_id)
        .await
        .map_err(fail)?
    else {
        return Ok(Err(Refusal::NotFound));
    };

    // What the picker may offer: this board's other tasks, minus the ones
    // already on either end of a link with this one.
    let mut linkable = Vec::new();
    if let Some(board) = store.board(&user.workspace_id).await.map_err(fail)? {
        let taken: Vec<&str> = detail
            .blocked_by
            .iter()
            .chain(detail.blocks.iter())
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

    Ok(Ok(DetailSnapshot {
        detail,
        linkable,
        may_write,
        may_comment: user.role.can_comment(),
        may_delete: may_write,
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
#[server]
pub async fn save_task(
    task_id: String,
    title: String,
    description: String,
    deadline: String,
) -> Result<Option<Refusal>, ServerFnError> {
    use crate::server::accounts;
    use time::OffsetDateTime;
    use time::macros::format_description;

    let (user, _) = match guard::writer_and_task(&task_id).await {
        Ok(pair) => pair,
        Err(refusal) => return Ok(Some(refusal)),
    };
    let title = title.trim().to_string();
    if title.is_empty() {
        return Ok(Some(Refusal::EmptyTitle));
    }
    let deadline = match deadline.trim() {
        "" => None,
        raw => match Date::parse(raw, format_description!("[year]-[month]-[day]")) {
            Ok(day) => Some(day),
            Err(_) => return Ok(Some(Refusal::BadDeadline)),
        },
    };

    accounts()
        .store()
        .save_task(
            &task_id,
            &title,
            description.trim(),
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
    use dizey_core::detail::ActivityKind;
    use time::OffsetDateTime;

    let (actor, _) = match guard::writer_and_task(&task_id).await {
        Ok(pair) => pair,
        Err(refusal) => return Ok(Some(refusal)),
    };
    let store = accounts().store().clone();
    let fail = |e: dizey_core::store::StoreError| ServerFnError::new(e.to_string());

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
    use dizey_core::detail::ActivityKind;
    use time::OffsetDateTime;

    let (actor, _) = match guard::writer_and_task(&task_id).await {
        Ok(pair) => pair,
        Err(refusal) => return Ok(Some(refusal)),
    };
    let store = accounts().store().clone();
    let fail = |e: dizey_core::store::StoreError| ServerFnError::new(e.to_string());

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
    use dizey_core::detail::ActivityKind;
    use dizey_core::store::StoreError;
    use time::OffsetDateTime;

    let (actor, _) = match guard::writer_and_task(&task_id).await {
        Ok(pair) => pair,
        Err(refusal) => return Ok(Some(refusal)),
    };
    let other = match guard::task_of(&actor, &other_id).await {
        Ok(facts) => facts,
        Err(refusal) => return Ok(Some(refusal)),
    };

    let (blocked, blocking) = match direction {
        Direction::BlockedBy => (task_id.clone(), other_id.clone()),
        Direction::Blocks => (other_id.clone(), task_id.clone()),
    };
    let store = accounts().store().clone();
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
    use dizey_core::detail::ActivityKind;
    use time::OffsetDateTime;

    let (actor, _) = match guard::writer_and_task(&task_id).await {
        Ok(pair) => pair,
        Err(refusal) => return Ok(Some(refusal)),
    };
    let other = match guard::task_of(&actor, &other_id).await {
        Ok(facts) => facts,
        Err(refusal) => return Ok(Some(refusal)),
    };

    let (blocked, blocking) = match direction {
        Direction::BlockedBy => (task_id.clone(), other_id.clone()),
        Direction::Blocks => (other_id.clone(), task_id.clone()),
    };
    let store = accounts().store().clone();
    let now = OffsetDateTime::now_utc();
    let fail = |e: dizey_core::store::StoreError| ServerFnError::new(e.to_string());
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
    if let Err(refusal) = guard::task_of(&user, &task_id).await {
        return Ok(Some(refusal));
    }
    let body = body.trim();
    if body.is_empty() {
        return Ok(Some(Refusal::EmptyComment));
    }

    accounts()
        .store()
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
/// recoverable by hand. Whatever was waiting only on it becomes unblocked, and
/// the store records that.
#[server]
pub async fn delete_task(task_id: String) -> Result<Option<Refusal>, ServerFnError> {
    use crate::server::accounts;
    use time::OffsetDateTime;

    let (user, _) = match guard::writer_and_task(&task_id).await {
        Ok(pair) => pair,
        Err(refusal) => return Ok(Some(refusal)),
    };
    accounts()
        .store()
        .delete_task(&task_id, &user.id, OffsetDateTime::now_utc())
        .await
        .map_err(|e| ServerFnError::new(e.to_string()))?;
    Ok(None)
}

// -- the screen -------------------------------------------------------------

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

    view! {
        <div
            class="modal-scrim"
            on:click=move |_| on_close()
            on:keydown=move |event| {
                if event.key() == "Escape" {
                    on_close();
                }
            }
        >
            <div
                class="modal"
                tabindex="-1"
                autofocus
                on:click=move |event| event.stop_propagation()
                on:keydown=move |event| {
                    if event.key() == "Escape" {
                        on_close();
                    }
                }
            >
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
                                    <p class="modal-note">"Something went wrong. Reload the page."</p>
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

#[component]
fn DetailScreen(
    snapshot: DetailSnapshot,
    on_close: impl Fn() + Copy + Send + Sync + 'static,
    on_change: impl Fn() + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let DetailSnapshot {
        detail,
        me: _,
        today,
        linkable,
        may_write,
        may_comment,
        may_delete,
    } = snapshot;

    let id = StoredValue::new(detail.id.clone());
    let task_key = detail.task_key.clone();
    let title = detail.title.clone();
    let title_field = detail.title.clone();
    let description = detail.description.clone();
    let description_field = detail.description.clone();
    let status = detail.column.name.clone();
    let deadline_input = detail.deadline_input();
    let overdue = detail.is_overdue(today);
    let deadline_label = detail.deadline_label(today);
    let assignee_count = detail.assignees.len();
    let unassigned: Vec<Person> = detail.unassigned().cloned().collect();

    let save = ServerAction::<SaveTask>::new();
    let comment = ServerAction::<PostComment>::new();
    let link = ServerAction::<LinkTasks>::new();
    let remove = ServerAction::<DeleteTask>::new();
    // Deleting is two steps: ask what it would cost, then say it out loud and
    // let the person decide. The artboard's red button had no confirmation and
    // this action reaches other people's tasks.
    let ask = ServerAction::<WhatDeleteCosts>::new();
    // Every action that lands re-reads the task and the board behind it.
    for value in [save.value(), comment.value(), link.value()] {
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

    let problem = move || {
        [save.value(), comment.value(), link.value(), remove.value()]
            .into_iter()
            .find_map(|value| value.get().and_then(|answer| answer.ok().flatten()))
            .map(|refusal| view! { <p class="modal-problem">{refusal.message()}</p> })
    };

    view! {
        <header class="detail-head">
            <span class="detail-key">{task_key}</span>
            <h2 class="detail-title">{title}</h2>
            <div class="spacer"></div>
            <span class="detail-esc">"esc"</span>
            <button class="detail-close" type="button" on:click=move |_| on_close()>
                "✕"
            </button>
        </header>

        <div class="detail-fields">
            <div class="detail-field">
                <span class="detail-label">"STATUS"</span>
                // Read-only this slice: moving a card is the transition the
                // mail rules watch, and it lands with the drag.
                <span class="detail-value">{status}</span>
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
                    {may_write
                        .then(|| {
                            view! { <AssigneePicker task_id=id people=unassigned on_change=on_change/> }
                        })}
                </div>
            </div>
            <div class="detail-field">
                <span class="detail-label">"DEADLINE"</span>
                <span class="detail-value" class:detail-overdue=overdue>
                    {deadline_label}
                </span>
            </div>
        </div>

        <ActionForm action=save attr:class="detail-form">
            <input type="hidden" name="task_id" value=move || id.get_value()/>
            <span class="detail-label">"TITLE"</span>
            <input
                class="detail-input"
                type="text"
                name="title"
                value=title_field
                required
                disabled=!may_write
            />
            <span class="detail-label">"DESCRIPTION"</span>
            <textarea
                class="detail-textarea"
                name="description"
                rows="4"
                disabled=!may_write
                prop:value=description_field
            >
                {description}
            </textarea>
            <span class="detail-label">"DEADLINE"</span>
            <input
                class="detail-input detail-date"
                type="date"
                name="deadline"
                value=deadline_input
                disabled=!may_write
            />
            {may_write
                .then(|| {
                    view! {
                        <div class="detail-form-row">
                            <button
                                class="detail-save"
                                type="submit"
                                disabled=move || save.pending().get()
                            >
                                "Save"
                            </button>
                        </div>
                    }
                })}
        </ActionForm>

        <section class="detail-section">
            <div class="detail-section-head">
                <span class="detail-label">"DEPENDENCIES"</span>
            </div>
            {(!detail.blocked_by.is_empty())
                .then(|| {
                    view! {
                        <div class="dep-group">
                            <span class="dep-heading">"BLOCKED BY"</span>
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
                        </div>
                    }
                })}
            {(!detail.blocks.is_empty())
                .then(|| {
                    view! {
                        <div class="dep-group">
                            <span class="dep-heading">"BLOCKS"</span>
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
            {(may_write && !linkable.is_empty())
                .then(|| {
                    view! {
                        <ActionForm action=link attr:class="dep-link">
                            <input type="hidden" name="task_id" value=move || id.get_value()/>
                            <select class="dep-select" name="other_id">
                                {linkable
                                    .iter()
                                    .map(|target| {
                                        view! {
                                            <option value=target.id.clone()>
                                                {format!("{} {}", target.task_key, target.title)}
                                            </option>
                                        }
                                    })
                                    .collect_view()}
                            </select>
                            <select class="dep-select" name="direction">
                                <option value="blocked_by">"blocks this task"</option>
                                <option value="blocks">"waits on this task"</option>
                            </select>
                            <button class="dep-add" type="submit" disabled=move || link.pending().get()>
                                "Link a task"
                            </button>
                        </ActionForm>
                    }
                })}
        </section>

        <section class="detail-section">
            <span class="detail-label">"COMMENTS"</span>
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
                    }
                })}
        </section>

        <section class="detail-section">
            <span class="detail-label">"ACTIVITY"</span>
            {detail
                .activity
                .iter()
                .map(|entry| {
                    let who = entry
                        .actor
                        .as_ref()
                        .map(|person| person.display_name.clone())
                        .unwrap_or_else(|| "Dizey".to_string());
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

        {problem}

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
                                "The task keeps its record in the database, but nothing in Dizey brings it back."
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
                                "Delete task"
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

    view! {
        <span class="assignee-chip">
            {name.clone()}
            {may_write
                .then(|| {
                    view! {
                        <ActionForm action=action attr:class="assignee-drop">
                            <input type="hidden" name="task_id" value=move || task_id.get_value()/>
                            <input type="hidden" name="user_id" value=person.id.clone()/>
                            <button
                                class="assignee-remove"
                                type="submit"
                                title=format!("Take {name} off this task")
                            >
                                "✕"
                            </button>
                        </ActionForm>
                    }
                })}
        </span>
    }
}

#[component]
fn AssigneePicker(
    task_id: StoredValue<String>,
    people: Vec<Person>,
    on_change: impl Fn() + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let action = ServerAction::<Assign>::new();
    let value = action.value();
    Effect::new(move |_| {
        if matches!(value.get(), Some(Ok(None))) {
            on_change();
        }
    });
    let empty = people.is_empty();

    view! {
        {(!empty)
            .then(|| {
                view! {
                    <ActionForm action=action attr:class="assignee-add">
                <input type="hidden" name="task_id" value=move || task_id.get_value()/>
                <select class="assignee-select" name="user_id">
                    {people
                        .iter()
                        .map(|person| {
                            view! {
                                <option value=person.id.clone()>{person.display_name.clone()}</option>
                            }
                        })
                        .collect_view()}
                </select>
                    <button class="assignee-assign" type="submit">
                        "Assign"
                    </button>
                    </ActionForm>
                }
            })}
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
    let waiting = matches!(direction, Direction::Blocks);
    let other_id = edge.task_id.clone();
    let wire = match direction {
        Direction::BlockedBy => "blocked_by",
        Direction::Blocks => "blocks",
    };

    view! {
        <div class="dep-row" class:dep-row-waiting=waiting>
            {cleared.then(|| view! { <span class="dep-tick">"✓"</span> })}
            <span class="dep-key">{edge.task_key.clone()}</span>
            <span class="dep-title">{edge.title.clone()}</span>
            <div class="spacer"></div>
            <span class="dep-note">{note}</span>
            {(may_write && !cleared)
                .then(|| {
                    view! {
                        <ActionForm action=action attr:class="dep-unlink-form">
                            <input type="hidden" name="task_id" value=move || task_id.get_value()/>
                            <input type="hidden" name="other_id" value=other_id/>
                            <input type="hidden" name="direction" value=wire/>
                            <button class="dep-unlink" type="submit" title="Remove this link">
                                "✕"
                            </button>
                        </ActionForm>
                    }
                })}
        </div>
    }
}

#[component]
fn CommentRow(comment: Comment) -> impl IntoView {
    use dizey_core::detail::moment_label;

    view! {
        <div class="comment">
            <div class="comment-head">
                <span class="comment-who">{comment.author.display_name.clone()}</span>
                <span class="comment-when">{moment_label(comment.at)}</span>
            </div>
            <div class="comment-body">{comment.body.clone()}</div>
        </div>
    }
}
