//! The board, from the Main artboard.
//!
//! The whole column list is one shard: the server renders every column and
//! card, and a drop re-runs it in place. There is no client board state to
//! keep in sync with the server — a drag's only client-side memory is the
//! browser's own drag session (`dataTransfer`), read back by the column that
//! catches the drop.

use izlek_core::board::{Moved, Person, TaskCard};
use izlek_core::store::{NewTask, User};
use time::{Date, OffsetDateTime};
use topcoat::Result;
use topcoat::context::Cx;
use topcoat::router::content::Form;
use topcoat::router::request::headers;
use topcoat::router::{HeaderName, StatusCode, header, query_params, route};
use topcoat::runtime::{Event, Surrogated, procedure, shard};
use topcoat::view::view;

use crate::server::{Refusal, accounts, mail, require_user, require_writer};

/// The board a task belongs to, checked against this person's workspace
/// before anything trusts an id the browser sent.
async fn task_board_id(cx: &Cx, user: &User, task_id: &str) -> Result<Option<String>> {
    let store = accounts(cx).store().clone();
    match store.task(task_id).await? {
        Some(facts) if facts.workspace_id == user.workspace_id => Ok(Some(facts.board_id)),
        _ => Ok(None),
    }
}

/// Moves a card. The one path `/api/move_card` and the drop procedure both
/// call, so the two cannot drift.
async fn move_card_shared(
    cx: &Cx,
    task_id: &str,
    from_column_id: &str,
    to_column_id: &str,
) -> Result<Option<Refusal>> {
    let user = match require_writer(cx).await {
        Ok(user) => user,
        Err(refusal) => return Ok(Some(refusal)),
    };
    let Some(board_id) = task_board_id(cx, &user, task_id).await? else {
        return Ok(Some(Refusal::NotFound));
    };
    let store = accounts(cx).store().clone();
    let columns = store.columns(&board_id).await?;
    if !columns.iter().any(|column| column.id == to_column_id) {
        return Ok(Some(Refusal::Forbidden));
    }
    match store
        .move_task(task_id, from_column_id, to_column_id, &user.id, OffsetDateTime::now_utc())
        .await
    {
        Ok(Moved::Recorded(transition)) => {
            mail(cx).after(transition);
            Ok(None)
        }
        Ok(Moved::Unchanged) => Ok(None),
        Ok(Moved::Stale) => Ok(Some(Refusal::MovedAlready)),
        Err(izlek_core::store::StoreError::NotFound) => Ok(Some(Refusal::NotFound)),
        Err(error) => {
            eprintln!("store error: {error}");
            Ok(Some(Refusal::Unavailable))
        }
    }
}

/// Adds a card to a column. A Viewer is refused here, in the handler.
async fn create_task_shared(cx: &Cx, title: &str, column_id: &str) -> Result<Option<Refusal>> {
    let user = match require_writer(cx).await {
        Ok(user) => user,
        Err(refusal) => return Ok(Some(refusal)),
    };
    let title = title.trim();
    if title.is_empty() {
        return Ok(Some(Refusal::EmptyTitle));
    }
    let store = accounts(cx).store().clone();
    let Some(board) = store.board(&user.workspace_id).await? else {
        return Ok(Some(Refusal::Unavailable));
    };
    let columns = store.columns(&board.id).await?;
    if !columns.iter().any(|column| column.id == column_id) {
        return Ok(Some(Refusal::Forbidden));
    }
    let created = store
        .create_task(NewTask {
            board_id: &board.id,
            column_id,
            title,
            description: "",
            deadline: None,
            created_by: &user.id,
        })
        .await?;
    mail(cx).after_activity(store, created.activity_id);
    mail(cx).after(created.transition);
    Ok(None)
}

type Redirect = Result<(StatusCode, [(HeaderName, String); 1])>;

fn redirect(cx: &Cx) -> Redirect {
    let back = headers(cx)
        .get(header::REFERER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("/")
        .to_string();
    Ok((StatusCode::SEE_OTHER, [(header::LOCATION, back)]))
}

/// Back to the referer, with the refusal on the query the way `settings.rs`
/// carries one, so a browser without script learns why a create or move did
/// not happen.
fn redirect_refused(cx: &Cx, call: &str, refusal: Refusal) -> Redirect {
    let back = headers(cx)
        .get(header::REFERER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("/")
        .to_string();
    let separator = if back.contains('?') { '&' } else { '?' };
    let location = format!("{back}{separator}refusal={}&on={call}", refusal.code());
    Ok((StatusCode::SEE_OTHER, [(header::LOCATION, location)]))
}

#[derive(serde::Deserialize)]
struct CreateTaskForm {
    title: String,
    column_id: String,
}

/// A browser without script's way onto the board: a real form post, same
/// fields the procedure below trades over the wire.
#[route(POST "/api/create_task")]
async fn create_task(cx: &Cx, Form(input): Form<CreateTaskForm>) -> Redirect {
    match create_task_shared(cx, &input.title, &input.column_id).await? {
        Some(refusal) => redirect_refused(cx, "create_task", refusal),
        None => redirect(cx),
    }
}

#[derive(serde::Deserialize)]
struct MoveCardForm {
    task_id: String,
    from_column_id: String,
    to_column_id: String,
}

/// The same move the drop procedure performs, reachable without script.
#[route(POST "/api/move_card")]
async fn move_card(cx: &Cx, Form(input): Form<MoveCardForm>) -> Redirect {
    match move_card_shared(cx, &input.task_id, &input.from_column_id, &input.to_column_id).await? {
        Some(refusal) => redirect_refused(cx, "move_card", refusal),
        None => redirect(cx),
    }
}

/// What a drop calls: performs the move and hands back a refusal code, if
/// any, for the drop handler to leave alone. Callable with any arguments —
/// the checks above are what actually guard it.
#[procedure]
async fn move_card_procedure(
    cx: &Cx,
    task_id: String,
    from_column_id: String,
    to_column_id: String,
) -> Result<Result<bool, String>> {
    match move_card_shared(cx, &task_id, &from_column_id, &to_column_id).await? {
        Some(refusal) => Ok(Err(refusal.code().to_string())),
        None => Ok(Ok(true)),
    }
}

/// `deadline`, `created` or `title` — the "Sort" control in the filter bar.
/// Anything else (a hand-edited or stale query string) falls back to
/// `deadline` silently.
const SORT_KEYS: [&str; 3] = ["deadline", "created", "title"];

fn valid_sort(sort: Option<&str>) -> &'static str {
    match sort {
        Some("created") => "created",
        Some("title") => "title",
        _ => "deadline",
    }
}

/// `deadline` is the order [`izlek_core::board::load`] already hands back
/// (soonest first); the other two re-order in place here so core stays free
/// of a UI sort preference. `created` reads the task id, a ULID, which sorts
/// lexically by the moment it was minted — newest first.
fn sort_column_cards(cards: &mut [TaskCard], sort: &str) {
    match sort {
        "created" => cards.sort_by(|a, b| b.id.cmp(&a.id)),
        "title" => cards.sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase())),
        _ => {}
    }
}

/// The whole column list. `version` carries no meaning of its own — bumping
/// it is what tells the browser to ask for this again after a drop lands.
#[shard]
async fn board_columns(cx: &Cx, version: f64, sort: String) -> Result {
    let _ = version;
    let user = match require_user(cx).await {
        Ok(user) => user,
        Err(refusal) => {
            return view! {
                cx =>
                <div class="scaffold-note"><p>(refusal.message())</p></div>
            };
        }
    };
    let store = accounts(cx).store().clone();
    let Some(mut view_data) = izlek_core::board::load(store.as_ref(), &user.workspace_id).await?
    else {
        return view! {
            cx =>
            <div class="scaffold-note"><p>"Something went wrong."</p></div>
        };
    };
    for column in &mut view_data.columns {
        sort_column_cards(&mut column.cards, &sort);
    }
    let today = OffsetDateTime::now_utc().date();
    let may_write = user.role.can_write_tasks();
    let empty = view_data.is_empty();

    let all_columns: Vec<(String, String)> =
        view_data.columns.iter().map(|column| (column.column.id.clone(), column.column.name.clone())).collect();
    let mut columns = Vec::new();
    for column in view_data.columns {
        columns.push(render_column(cx, column, today, may_write, &all_columns).await?);
    }

    view! {
        cx =>
        for column in columns { (column) }
        if empty {
            <div class="board-empty">
                <div class="board-empty-title">"This board is empty"</div>
            </div>
        }
    }
}

async fn render_column(
    cx: &Cx,
    column: izlek_core::board::ColumnView,
    today: Date,
    may_write: bool,
    all_columns: &[(String, String)],
) -> Result {
    let column_id = column.column.id;
    let name = column.column.name;
    let is_done_column = column.column.is_done;
    let count = column.cards.len();
    let mut cards = Vec::new();
    for card in column.cards {
        cards.push(render_card(cx, card, today, is_done_column, &column_id, may_write, all_columns).await?);
    }

    view! {
        cx =>
        signal composing = false;
        <section class="column" id=(column_id.clone())>
            <header class="column-head">
                <span class="column-name">(name)</span>
                <span class="column-count">(count)</span>
                <div class="spacer"></div>
                if may_write {
                    <button class="column-add" title="Add a task" @click=$(|_e: Event| composing.set(true))>
                        "+"
                    </button>
                }
            </header>
            <div class="column-cards" id=(column_id.clone())>
                for card in cards { (card) }
                if may_write {
                    <form method="post" action="/api/create_task" class="composer" :hidden=$(!composing.get())>
                        <input type="hidden" name="column_id" value=(column_id.clone())>
                        <input class="composer-input" type="text" name="title" placeholder="What needs doing?" required="">
                        <div class="composer-row">
                            <button class="composer-add" type="submit">"Add"</button>
                            <button class="composer-cancel" type="button" @click=$(|_e: Event| composing.set(false))>
                                "Cancel"
                            </button>
                        </div>
                    </form>
                }
            </div>
        </section>
    }
}

// The `@dragstart`/`@contextmenu` handlers below never read `e` as ordinary
// Rust — the `raw!` macro resolves `${e}` through its own name table, keyed
// on the closure param's identifier text, not through the generated closure
// body.
#[allow(unused_variables)]
async fn render_card(
    cx: &Cx,
    card: TaskCard,
    today: Date,
    done_column: bool,
    column_id: &str,
    may_write: bool,
    all_columns: &[(String, String)],
) -> Result {
    let blocks = card.blocks.len();
    let blocked_by = card.blocked_by.join(", ");
    let deadline = card.deadline_label(today);
    let overdue = card.is_overdue(today);
    let dated = card.deadline.is_some() || card.is_done();
    let comments = card.comment_count;
    let mut assignees = Vec::new();
    for person in card.assignees.iter() {
        assignees.push(avatar(cx, person, "").await?);
    }
    let has_assignees = !card.assignees.is_empty();
    let task_id = card.id.clone();
    let from_column = column_id.to_string();
    let href = format!("/?task={}", card.id);
    let menu_id = format!("card-menu-{}", card.id);
    let move_targets: Vec<(String, String)> =
        all_columns.iter().filter(|(id, _)| id != column_id).cloned().collect();
    // `raw!` interpolation only takes a bare identifier, and it moves what it
    // takes — one clone per site that also needs the value afterward.
    let drag_task_id = task_id.clone();
    let drag_from_column = from_column.clone();
    let open_menu_id = menu_id.clone();

    view! {
        cx =>
        <a class="card" id=(column_id.to_string()) class:card-done=(done_column) href=(href.clone())
            draggable=(if may_write { "true" } else { "false" })
            @dragstart=$(|e: Event| raw!(
                "(() => { ${e}.inner.dataTransfer.setData('izlek-task', ${drag_task_id}); ${e}.inner.dataTransfer.setData('izlek-from', ${drag_from_column}); })()",
                ()
            ))
            @contextmenu=$(|e: Event| {
                e.prevent_default();
                raw!(
                    "(() => { window.__izlekOpenCardMenu(${e}.inner, ${open_menu_id}); })()",
                    ()
                )
            })
        >
            <div class="card-keys">
                <span class="card-key">(card.task_key.clone())</span>
                if blocks > 0 { <span class="card-blocks">(format!("blocks {blocks}"))</span> }
                if !blocked_by.is_empty() {
                    <span class="card-blocked-by">(format!("blocked by {blocked_by}"))</span>
                }
            </div>
            <div class="card-title">(card.title.clone())</div>
            <div class="card-foot">
                <span class="card-deadline" class:card-deadline-overdue=(overdue) class:card-deadline-none=(!dated)>
                    (deadline)
                </span>
                if comments > 0 { <span class="card-comments">(format!("{comments} \u{270e}"))</span> }
                <div class="spacer"></div>
                <div class="avatars">
                    for person in assignees { (person) }
                    if !has_assignees {
                        <span class="avatar avatar-none" role="img" aria-label="nobody assigned" title="Nobody assigned"></span>
                    }
                </div>
            </div>
        </a>
        <div class="card-menu pop-panel" id=(menu_id.clone())>
            <div class="pop-list">
                <a class="pop-row" href=(href.clone())>"Open"</a>
                if may_write && !move_targets.is_empty() {
                    <div class="card-menu-move">
                        <button class="pop-row card-menu-move-trigger" type="button">"Move"</button>
                        <div class="card-menu-submenu pop-panel">
                            <div class="pop-list">
                                for target in move_targets.iter() {
                                    <form class="pop-row-form" method="post" action="/api/move_card">
                                        <input type="hidden" name="task_id" value=(task_id.clone())>
                                        <input type="hidden" name="from_column_id" value=(from_column.clone())>
                                        <input type="hidden" name="to_column_id" value=(target.0.clone())>
                                        <button class="pop-row" type="submit">(target.1.clone())</button>
                                    </form>
                                }
                            </div>
                        </div>
                    </div>
                }
                if may_write {
                    <form class="pop-row-form" method="post" action="/api/delete_task">
                        <input type="hidden" name="task_id" value=(task_id.clone())>
                        <button class="pop-row card-menu-danger" type="submit">"Delete"</button>
                    </form>
                }
            </div>
        </div>
    }
}

/// Opens a card's context menu at the cursor and closes whichever one was
/// open; a plain document click closes it again. Rendered once — every
/// card's `@contextmenu` calls into this same global, by the menu's id.
async fn card_menu_script(cx: &Cx) -> Result {
    use topcoat::view::Unescaped;
    const JS: &str = "\
        function closeCardMenus() { document.querySelectorAll('.card-menu-open').forEach(function (el) { el.classList.remove('card-menu-open'); }); } \
        window.__izlekOpenCardMenu = function (e, id) { \
            closeCardMenus(); \
            var menu = document.getElementById(id); \
            if (!menu) { return; } \
            menu.style.left = e.clientX + 'px'; \
            menu.style.top = e.clientY + 'px'; \
            menu.classList.add('card-menu-open'); \
        }; \
        document.addEventListener('click', closeCardMenus);";
    view! { cx => <script>(Unescaped::new_unchecked(JS))</script> }
}

pub(crate) async fn avatar(cx: &Cx, person: &Person, extra: &str) -> Result {
    let initials = person.initials();
    let tone = person
        .id
        .bytes()
        .fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32))
        % 5;
    let class = format!("avatar avatar-tone-{tone} {extra}");
    let title = person.display_name.clone();
    view! {
        cx =>
        <span class=(class) title=(title)>(initials)</span>
    }
}

/// Which task, if any, `/` renders the detail modal open on, and the refusal
/// (if any) a create or move landed back here with.
#[query_params(error = redirect("/"))]
struct BoardQuery {
    task: Option<String>,
    refusal: Option<String>,
    on: Option<String>,
    sort: Option<String>,
}

/// The noun a sort key shows in the `<select>` — terse, no explainer.
fn sort_label(sort: &str) -> &'static str {
    match sort {
        "created" => "Created",
        "title" => "Title",
        _ => "Deadline",
    }
}

/// The signed-in board: the topbar, the filter chips and the shard that owns
/// every column and card.
#[allow(unused_variables)]
pub async fn board_page(cx: &Cx, user: &User) -> Result {
    let view_data = {
        let store = accounts(cx).store().clone();
        izlek_core::board::load(store.as_ref(), &user.workspace_id).await?
    };
    let Some(view_data) = view_data else {
        return view! {
            cx =>
            <main class="scaffold-note"><p>"Something went wrong."</p></main>
        };
    };
    let today = OffsetDateTime::now_utc().date();
    let overdue = view_data.overdue_count(today);
    let blocked = view_data.blocked_count();
    let query = query_params::<BoardQuery>(cx)?;
    let open_task = query.task.clone();
    let sort = valid_sort(query.sort.as_deref());
    let refusal = match (query.on.as_deref(), query.refusal.as_deref()) {
        (Some("create_task") | Some("move_card"), Some(code)) => Refusal::from_code(code),
        _ => None,
    };

    view! {
        cx =>
        <header class="topbar">
            <a class="wordmark" href="/">
                <span class="wordmark-text">"izlek"</span>
                <span class="wordmark-dot"></span>
            </a>
            <div class="spacer"></div>
            <span class="topbar-who" title=(user.email.clone())>(user.display_name.clone())</span>
            <a class="topbar-link" href="/settings">"Settings"</a>
        </header>
        <div class="filterbar">
            <div class="topbar-divider"></div>
            <form class="field-box" method="get" action="/">
                <span class="field-text">"Sort"</span>
                <select class="status-select" name="sort"
                    @change=$(|e: Event| raw!("${e}.inner.target.form.requestSubmit()", ()))
                >
                    for key in SORT_KEYS {
                        <option value=(key) selected=(key == sort)>(sort_label(key))</option>
                    }
                </select>
            </form>
            <div class="spacer"></div>
            if overdue > 0 { <span class="chip chip-overdue">(format!("{overdue} overdue"))</span> }
            if blocked > 0 { <span class="chip chip-blocked">(format!("{blocked} blocked"))</span> }
        </div>
        if let Some(refusal) = &refusal {
            <p class="field-error">(refusal.message())</p>
        }
        <main class="board-stage">
            signal version = 0.0;
            <div class="board-columns"
                @dragover=$(|e: Event| e.prevent_default())
                @drop=$(async move |e: Event| {
                    e.prevent_default();
                    let to = e.target.id.clone();
                    let task_id = raw!("cx.hydrate(${e}.inner.dataTransfer.getData('izlek-task'))", "".to_owned());
                    let from = raw!("cx.hydrate(${e}.inner.dataTransfer.getData('izlek-from'))", "".to_owned());
                    if !task_id.is_empty() {
                        let _result = move_card_procedure(task_id, from, to).await;
                        version.set(version.get() + 1.0);
                    }
                })
            >
                board_columns(version: $(version.get()), sort: $(sort.to_string().into_surrogate()))
            </div>
        </main>
        if let Some(task_id) = &open_task {
            (crate::detail::task_modal(cx, task_id).await?)
        }
        (card_menu_script(cx).await?)
    }
}

#[cfg(test)]
mod sort_tests {
    use super::{TaskCard, sort_column_cards, valid_sort};

    fn card(id: &str, title: &str) -> TaskCard {
        TaskCard {
            id: id.to_string(),
            task_key: id.to_string(),
            title: title.to_string(),
            column_id: "col".to_string(),
            deadline: None,
            done_at: None,
            position: 0.0,
            assignees: Vec::new(),
            comment_count: 0,
            blocked_by: Vec::new(),
            blocks: Vec::new(),
        }
    }

    #[test]
    fn deadline_is_left_alone() {
        let mut cards = vec![card("b", "Bravo"), card("a", "Alpha")];
        sort_column_cards(&mut cards, "deadline");
        assert_eq!(cards[0].id, "b");
        assert_eq!(cards[1].id, "a");
    }

    #[test]
    fn created_orders_newest_ulid_first() {
        let mut cards = vec![card("01AAAA", "old"), card("01ZZZZ", "new")];
        sort_column_cards(&mut cards, "created");
        assert_eq!(cards[0].id, "01ZZZZ");
        assert_eq!(cards[1].id, "01AAAA");
    }

    #[test]
    fn title_orders_case_insensitively() {
        let mut cards = vec![card("1", "zebra"), card("2", "Apple")];
        sort_column_cards(&mut cards, "title");
        assert_eq!(cards[0].title, "Apple");
        assert_eq!(cards[1].title, "zebra");
    }

    #[test]
    fn invalid_query_param_falls_back_to_deadline() {
        assert_eq!(valid_sort(Some("bogus")), "deadline");
        assert_eq!(valid_sort(None), "deadline");
        assert_eq!(valid_sort(Some("created")), "created");
    }
}
