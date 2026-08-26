//! The board, from the Main and EmptyBoard artboards.
//!
//! The whole screen is one server call: the columns, the cards and everything
//! hanging off them arrive together, already joined. What a Viewer may not do
//! is refused in the server functions below — the missing button is a courtesy,
//! not the guard.

use izlek_core::board::{BoardView, Person, TaskCard};
use leptos::prelude::*;
use serde::{Deserialize, Serialize};
use time::Date;

use crate::auth::{Me, Refusal};

/// One board screen's worth of state.
///
/// `today` comes from the server: whether a deadline has passed is not left to
/// whatever the browser's clock says.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BoardSnapshot {
    pub view: BoardView,
    pub me: Me,
    pub today: Date,
}

/// The board this browser may see. (The `#[server]` macro names a struct after
/// the function, so the call is `current_board` and the answer is a
/// `BoardSnapshot`.)
#[server]
pub async fn current_board() -> Result<Result<BoardSnapshot, Refusal>, ServerFnError> {
    use crate::server::{accounts, require_user};
    use izlek_core::board::load;
    use time::OffsetDateTime;

    let user = match require_user().await {
        Ok(user) => user,
        Err(refusal) => return Ok(Err(refusal)),
    };
    let store = accounts().store().clone();
    let Some(view) = load(store.as_ref(), &user.workspace_id)
        .await
        .map_err(|e| ServerFnError::new(e.to_string()))?
    else {
        return Ok(Err(Refusal::Unavailable));
    };
    Ok(Ok(BoardSnapshot {
        view,
        me: Me {
            id: user.id,
            display_name: user.display_name,
            email: user.email,
            role: user.role,
        },
        today: OffsetDateTime::now_utc().date(),
    }))
}

/// Adds a card to a column. A Viewer is refused here, in the handler.
#[server]
pub async fn create_task(
    title: String,
    column_id: String,
) -> Result<Option<Refusal>, ServerFnError> {
    use crate::server::{accounts, require_writer};
    use izlek_core::store::NewTask;

    let user = match require_writer().await {
        Ok(user) => user,
        Err(refusal) => return Ok(Some(refusal)),
    };
    let title = title.trim().to_string();
    if title.is_empty() {
        return Ok(Some(Refusal::EmptyTitle));
    }

    let store = accounts().store().clone();
    let fail = |e: izlek_core::store::StoreError| ServerFnError::new(e.to_string());
    let Some(board) = store.board(&user.workspace_id).await.map_err(fail)? else {
        return Ok(Some(Refusal::Unavailable));
    };
    // The column id arrives from the browser, so it is checked against this
    // workspace's board rather than trusted.
    let columns = store.columns(&board.id).await.map_err(fail)?;
    if !columns.iter().any(|column| column.id == column_id) {
        return Ok(Some(Refusal::Forbidden));
    }

    store
        .create_task(NewTask {
            board_id: &board.id,
            column_id: &column_id,
            title: &title,
            description: "",
            deadline: None,
            created_by: &user.id,
        })
        .await
        .map_err(fail)?;
    Ok(None)
}

/// Moves a card into a column and, with it, records the crossing.
///
/// `from_column_id` is the column the browser had the card in when the drag
/// started or the status control was opened. It travels with the request so
/// the store can refuse a drop that was decided against a board somebody else
/// has already changed, rather than silently overwriting their move.
#[server]
pub async fn move_card(
    task_id: String,
    from_column_id: String,
    to_column_id: String,
) -> Result<Option<Refusal>, ServerFnError> {
    use crate::detail::guard::writer_and_task;
    use crate::server::accounts;
    use izlek_core::board::Moved;
    use time::OffsetDateTime;

    let (user, facts) = match writer_and_task(&task_id).await {
        Ok(pair) => pair,
        Err(refusal) => return Ok(Some(refusal)),
    };
    // The destination is checked against this task's own board before the
    // store is asked, so a column id from another board is refused by the
    // handler and not merely by a foreign key.
    let store = accounts().store().clone();
    let fail = |e: izlek_core::store::StoreError| ServerFnError::new(e.to_string());
    let columns = store.columns(&facts.board_id).await.map_err(fail)?;
    if !columns.iter().any(|column| column.id == to_column_id) {
        return Ok(Some(Refusal::Forbidden));
    }

    match store
        .move_task(
            &task_id,
            &from_column_id,
            &to_column_id,
            &user.id,
            OffsetDateTime::now_utc(),
        )
        .await
    {
        // The crossing is committed by now. The rules read it afterwards and
        // send outside this call: the move has to stand whether or not a mail
        // server is reachable.
        Ok(Moved::Recorded(transition)) => {
            crate::server::mail().after(transition);
            Ok(None)
        }
        // A card dropped back where it came from is not news and is not an
        // error: the board simply re-reads and looks the same.
        Ok(Moved::Unchanged) => Ok(None),
        Ok(Moved::Stale) => Ok(Some(Refusal::MovedAlready)),
        Err(izlek_core::store::StoreError::NotFound) => Ok(Some(Refusal::NotFound)),
        Err(error) => {
            eprintln!("store error: {error}");
            Ok(Some(Refusal::Unavailable))
        }
    }
}

/// Which cards the filter row is showing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Filter {
    All,
    Mine,
    Blocked,
}

impl Filter {
    fn keeps(self, card: &TaskCard, me: &str) -> bool {
        match self {
            Filter::All => true,
            Filter::Mine => card.is_assigned_to(me),
            Filter::Blocked => card.is_blocked(),
        }
    }
}

#[component]
pub fn Board() -> impl IntoView {
    let board = Resource::new(|| (), |_| async move { current_board().await });
    // Which card is open in the detail modal, if any. It lives out here, above
    // the transition boundary: a refetch rebuilds `BoardScreen` from scratch, and
    // a signal owned by that component would be born empty again — closing the
    // modal every time something inside it saved.
    // Seeded from `?task=`, so a card is a link and the modal has an address. A
    // browser without script lands here with the modal already open; with script
    // the card's click handler wins and nothing navigates.
    let opened = RwSignal::new(
        leptos_router::hooks::use_query_map()
            .read_untracked()
            .get("task"),
    );

    view! {
        <Transition fallback=|| {
            view! { <main class="board-stage"></main> }
        }>
            {move || Suspend::new(async move {
                match board.await {
                    Ok(Ok(snapshot)) => {
                        view! {
                            <BoardScreen
                                snapshot=snapshot
                                opened=opened
                                on_change=move || board.refetch()
                            />
                        }
                            .into_any()
                    }
                    Ok(Err(refusal)) => {
                        view! {
                            <main class="scaffold-note">
                                <p>{refusal.message()}</p>
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
fn BoardScreen(
    snapshot: BoardSnapshot,
    opened: RwSignal<Option<String>>,
    on_change: impl Fn() + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let BoardSnapshot { view, me, today } = snapshot;
    let filter = RwSignal::new(Filter::All);
    // Which column has its composer open, if any.
    let composing = RwSignal::new(None::<String>);
    // The card currently in the air, and the column it was picked up from.
    // The column it came from is the drag's claim about the board it started
    // on, and it travels with the drop so a move decided against a stale board
    // is refused rather than applied.
    let dragging = RwSignal::new(None::<(String, String)>);
    let moving = ServerAction::<MoveCard>::new();
    let moved = moving.value();
    Effect::new(move |_| {
        if matches!(moved.get(), Some(Ok(_))) {
            on_change();
        }
    });
    let may_write = me.role.can_write_tasks();
    let my_id = StoredValue::new(me.id.clone());

    // The New task button opens the composer in the first column, which is
    // where a new card belongs when nobody has said otherwise.
    let first_column_id =
        StoredValue::new(view.columns.first().map(|column| column.column.id.clone()));

    let overdue = view.overdue_count(today);
    let blocked = view.blocked_count();
    let empty = view.is_empty();
    let board_name = view.board.name.clone();

    let columns = view
        .columns
        .into_iter()
        .map(|column| {
            // Both live in stored values so the closures below stay `Copy`:
            // a column's cards and its id are read by the count, the list, the
            // composer and the empty-column button.
            let cards = StoredValue::new(column.cards);
            let column_id = StoredValue::new(column.column.id.clone());
            let name = column.column.name.clone();
            let is_done_column = column.column.is_done;
            let shown = Memo::new(move |_| {
                let filter = filter.get();
                let me = my_id.read_value().clone();
                cards
                    .read_value()
                    .iter()
                    .filter(|card| filter.keeps(card, &me))
                    .cloned()
                    .collect::<Vec<TaskCard>>()
            });
            let open_composer = move |_| composing.set(Some(column_id.read_value().clone()));
            let is_composing =
                move || composing.get().as_deref() == Some(column_id.read_value().as_str());

            view! {
                <section class="column">
                    <header class="column-head">
                        <span class="column-name">{name}</span>
                        <span class="column-count">{move || shown.get().len()}</span>
                        <div class="spacer"></div>
                        {may_write
                            .then(|| {
                                view! {
                                    <button
                                        class="column-add"
                                        title="Add a task"
                                        on:click=open_composer
                                    >
                                        "+"
                                    </button>
                                }
                            })}
                    </header>
                    <div
                        class="column-cards"
                        class:column-drop=move || {
                            dragging
                                .get()
                                .is_some_and(|(_, from)| from != *column_id.read_value())
                        }
                        on:dragover=move |event| {
                            // Saying "yes, you may drop here" is what
                            // preventing the default on dragover means.
                            if dragging.get().is_some() {
                                event.prevent_default();
                            }
                        }
                        on:drop=move |event| {
                            event.prevent_default();
                            if let Some((task_id, from)) = dragging.get() {
                                dragging.set(None);
                                let to = column_id.read_value().clone();
                                if from != to {
                                    moving
                                        .dispatch(MoveCard {
                                            task_id,
                                            from_column_id: from,
                                            to_column_id: to,
                                        });
                                }
                            }
                        }
                    >
                        <For each=move || shown.get() key=|card: &TaskCard| card.id.clone() let:card>
                            <Card
                                card=card
                                today=today
                                done_column=is_done_column
                                draggable=may_write
                                dragging=dragging
                                on_open=move |task_id| opened.set(Some(task_id))
                            />
                        </For>
                        <Show when=move || may_write && is_composing()>
                            <Composer
                                column_id=column_id
                                on_done=move || {
                                    composing.set(None);
                                    on_change();
                                }
                                on_cancel=move || composing.set(None)
                            />
                        </Show>
                        <Show when=move || {
                            may_write && !is_composing() && shown.get().is_empty()
                        }>
                            <button class="column-empty-add" on:click=open_composer>
                                "+ Add a task"
                            </button>
                        </Show>
                    </div>
                </section>
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
            <span class="board-name">{board_name}</span>
            <div class="spacer"></div>
            <span class="topbar-who" title=me.email.clone()>
                {me.display_name.clone()}
            </span>
            <a class="topbar-link" href="/settings">
                "Settings"
            </a>
            {may_write
                .then(|| {
                    view! {
                        <button
                            class="new-task"
                            on:click=move |_| composing.set(first_column_id.get_value())
                        >
                            "New task"
                        </button>
                    }
                })}
        </header>

        <div class="filterbar">
            <div class="segmented">
                <FilterTab filter=filter this=Filter::All label="All tasks"/>
                <FilterTab filter=filter this=Filter::Mine label="Mine"/>
                <FilterTab filter=filter this=Filter::Blocked label="Blocked"/>
            </div>
            <div class="topbar-divider"></div>
            <span class="sort-note">"Sort: deadline"</span>
            <div class="spacer"></div>
            {(overdue > 0)
                .then(|| view! { <span class="chip chip-overdue">{format!("{overdue} overdue")}</span> })}
            {(blocked > 0)
                .then(|| view! { <span class="chip chip-blocked">{format!("{blocked} blocked")}</span> })}
        </div>

        <main class="board-stage">
            <div class="board-columns">{columns}</div>
            {empty
                .then(|| {
                    view! {
                        <div class="board-empty">
                    <div class="board-empty-title">"This board is empty"</div>
                        </div>
                    }
                })}
            {move || {
                opened
                    .get()
                    .map(|task_id| {
                        view! {
                            <crate::detail::TaskDetailModal
                                task_id=task_id
                                on_close=move || opened.set(None)
                                on_change=on_change
                            />
                        }
                    })
            }}
        </main>
    }
}

#[component]
fn FilterTab(filter: RwSignal<Filter>, this: Filter, label: &'static str) -> impl IntoView {
    view! {
        <button
            class="segment"
            class:segment-on=move || filter.get() == this
            on:click=move |_| filter.set(this)
        >
            {label}
        </button>
    }
}

#[component]
fn Card(
    card: TaskCard,
    today: Date,
    done_column: bool,
    /// A viewer cannot move work, so a viewer's cards do not lift.
    draggable: bool,
    dragging: RwSignal<Option<(String, String)>>,
    on_open: impl Fn(String) + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let blocks = card.blocks.len();
    let blocked_by = card.blocked_by.join(", ");
    let deadline = card.deadline_label(today);
    let overdue = card.is_overdue(today);
    let dated = card.deadline.is_some() || card.is_done();
    let comments = card.comment_count;
    let assignees = card.assignees.clone();
    let id = StoredValue::new(card.id.clone());
    let from_column = StoredValue::new(card.column_id.clone());
    let open = move || on_open(id.get_value());

    view! {
        // The whole card is the way in: the artboard has no separate control,
        // so the card itself carries the role and answers the keyboard.
        <a
            class="card"
            class:card-done=done_column
            href=format!("/?task={}", card.id)
            draggable=draggable.then_some("true")
            on:dragstart=move |_| {
                dragging.set(Some((id.get_value(), from_column.get_value())));
            }
            on:dragend=move |_| dragging.set(None)
            on:click=move |event| {
                event.prevent_default();
                open();
            }
            on:keydown=move |event| {
                if event.key() == " " {
                    event.prevent_default();
                    open();
                }
            }
        >
            <div class="card-keys">
                <span class="card-key">{card.task_key.clone()}</span>
                {(blocks > 0)
                    .then(|| view! { <span class="card-blocks">{format!("blocks {blocks}")}</span> })}
                {(!blocked_by.is_empty())
                    .then(|| {
                        view! {
                            <span class="card-blocked-by">{format!("blocked by {blocked_by}")}</span>
                        }
                    })}
            </div>
            <div class="card-title">{card.title.clone()}</div>
            <div class="card-foot">
                <span
                    class="card-deadline"
                    class:card-deadline-overdue=overdue
                    class:card-deadline-none=!dated
                >
                    {deadline}
                </span>
                {(comments > 0)
                    .then(|| view! { <span class="card-comments">{format!("{comments} ✎")}</span> })}
                <div class="spacer"></div>
                <div class="avatars">
                    {assignees
                        .iter()
                        .map(|person| view! { <Avatar person=person.clone()/> })
                        .collect_view()}
                    {assignees
                        .is_empty()
                        .then(|| {
                            view! {
                                // An empty circle, the avatar's size, in the
                                // avatar's place: the card's bottom row does
                                // not reflow when someone is assigned.
                                <span
                                    class="avatar avatar-none"
                                    role="img"
                                    aria-label="nobody assigned"
                                    title="Nobody assigned"
                                ></span>
                            }
                        })}
                </div>
            </div>
        </a>
    }
}

/// A person as a circle. `extra` carries the size the surface wants — the
/// board's own 22px is the default, the modal asks for its 18px and 28px.
#[component]
pub(crate) fn Avatar(person: Person, #[prop(optional)] extra: &'static str) -> impl IntoView {
    let initials = person.initials();
    // Five tones from the mockups' palette, picked from the id so a person
    // keeps the same one wherever they appear.
    let tone = person
        .id
        .bytes()
        .fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32))
        % 5;
    view! {
        <span class=format!("avatar avatar-tone-{tone} {extra}") title=person.display_name.clone()>
            {initials}
        </span>
    }
}

/// The inline "Add a task" input. It is a real form, so it still works with the
/// wasm bundle blocked.
#[component]
fn Composer(
    column_id: StoredValue<String>,
    on_done: impl Fn() + Copy + Send + Sync + 'static,
    on_cancel: impl Fn() + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let action = ServerAction::<CreateTask>::new();
    let value = action.value();
    let refusal = crate::auth::refusal_of(action);
    Effect::new(move |_| {
        if matches!(value.get(), Some(Ok(None))) {
            on_done();
        }
    });

    view! {
        <ActionForm action=action attr:class="composer">
            <input type="hidden" name="column_id" value=move || column_id.get_value()/>
            <input
                class="composer-input"
                type="text"
                name="title"
                placeholder="What needs doing?"
                autofocus
                required
                on:keydown=move |event| {
                    if event.key() == "Escape" {
                        on_cancel();
                    }
                }
            />
            <div class="composer-row">
                <button class="composer-add" type="submit" disabled=move || action.pending().get()>
                    "Add"
                </button>
                <button class="composer-cancel" type="button" on:click=move |_| on_cancel()>
                    "Cancel"
                </button>
            </div>
            {move || refusal().map(|refusal| view! { <p class="auth-problem">{refusal.message()}</p> })}
        </ActionForm>
    }
}
