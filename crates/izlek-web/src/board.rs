//! The board, from the Main artboard.
//!
//! The server renders every column and card; a drop posts `/api/move_card`
//! over `fetch` and swaps the redirected page in via `layout.rs`'s
//! `soft_nav_script`. There is no client board state to keep in sync with
//! the server — a drag's only client-side memory is the browser's own drag
//! session (`dataTransfer`), read back by the column that catches the drop.

use izlek_core::board::{DeadlineState, Moved, Person, TaskCard};
use izlek_core::store::{NewTask, User};
use time::{Date, OffsetDateTime};
use topcoat::Result;
use topcoat::context::Cx;
use topcoat::router::content::Form;
use topcoat::router::request::headers;
use topcoat::router::{HeaderName, StatusCode, header, query_params, route};
use topcoat::view::view;

use crate::i18n::{Key, Lang, t};
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
async fn create_task_shared(
    cx: &Cx,
    title: &str,
    column_id: &str,
    description: &str,
    deadline_raw: &str,
) -> Result<Option<Refusal>> {
    use time::macros::format_description;

    let user = match require_writer(cx).await {
        Ok(user) => user,
        Err(refusal) => return Ok(Some(refusal)),
    };
    let title = title.trim();
    if title.is_empty() {
        return Ok(Some(Refusal::EmptyTitle));
    }
    let deadline = match deadline_raw.trim() {
        "" => None,
        day => match Date::parse(day, format_description!("[year]-[month]-[day]")) {
            Ok(day) => Some(day),
            Err(_) => return Ok(Some(Refusal::BadDeadline)),
        },
    };
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
            description: description.trim(),
            deadline,
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
    #[serde(default)]
    description: String,
    #[serde(default)]
    deadline: String,
}

/// A browser without script's way onto the board: a real form post, same
/// fields the procedure below trades over the wire.
#[route(POST "/api/create_task")]
async fn create_task(cx: &Cx, Form(input): Form<CreateTaskForm>) -> Redirect {
    match create_task_shared(cx, &input.title, &input.column_id, &input.description, &input.deadline).await? {
        Some(refusal) => redirect_refused(cx, "create_task", refusal),
        // Created; the referring `/?new=1` would reopen a blank new-task
        // modal, so land on the board itself instead.
        None => Ok((StatusCode::SEE_OTHER, [(header::LOCATION, "/".to_string())])),
    }
}

#[derive(serde::Deserialize)]
struct MoveCardForm {
    task_id: String,
    from_column_id: String,
    to_column_id: String,
}

/// The one move path: the modal's status form, a card menu's "Move" rows
/// and the drop handler in `card_menu_script` all post here.
#[route(POST "/api/move_card")]
async fn move_card(cx: &Cx, Form(input): Form<MoveCardForm>) -> Redirect {
    match move_card_shared(cx, &input.task_id, &input.from_column_id, &input.to_column_id).await? {
        Some(refusal) => redirect_refused(cx, "move_card", refusal),
        None => redirect(cx),
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

/// The whole column list.
async fn board_columns(cx: &Cx, sort: String) -> Result {
    let user = match require_user(cx).await {
        Ok(user) => user,
        Err(refusal) => {
            // No session to read a language off of — English, same as an
            // auth screen with no user yet.
            return view! {
                cx =>
                <div class="scaffold-note"><p>(refusal.message())</p></div>
            };
        }
    };
    let lang = Lang::from_code(&user.language);
    let store = accounts(cx).store().clone();
    let Some(mut view_data) = izlek_core::board::load(store.as_ref(), &user.workspace_id).await?
    else {
        return view! {
            cx =>
            <div class="scaffold-note"><p>(t(lang, Key::SomethingWentWrong))</p></div>
        };
    };
    for column in &mut view_data.columns {
        sort_column_cards(&mut column.cards, &sort);
    }
    let today = OffsetDateTime::now_utc().date();
    let may_write = user.role.can_write_tasks();

    let all_columns: Vec<(String, String)> =
        view_data.columns.iter().map(|column| (column.column.id.clone(), column.column.name.clone())).collect();
    let mut columns = Vec::new();
    for column in view_data.columns {
        columns.push(render_column(cx, column, today, may_write, &all_columns, lang).await?);
    }

    view! {
        cx =>
        for column in columns { (column) }
    }
}

async fn render_column(
    cx: &Cx,
    column: izlek_core::board::ColumnView,
    today: Date,
    may_write: bool,
    all_columns: &[(String, String)],
    lang: Lang,
) -> Result {
    let column_id = column.column.id;
    let name = column.column.name;
    let is_done_column = column.column.is_done;
    let count = column.cards.len();
    let is_empty = column.cards.is_empty();
    let mut cards = Vec::new();
    for card in column.cards {
        cards.push(render_card(cx, card, today, is_done_column, &column_id, may_write, all_columns, lang).await?);
    }

    view! {
        cx =>
        <section class="column">
            <header class="column-head">
                <span class="column-name">(name)</span>
                <span class="column-count">(count)</span>
            </header>
            <div class="column-cards" id=(column_id.clone())>
                if is_empty { <div class="column-empty">(t(lang, Key::NoTasks))</div> }
                for card in cards { (card) }
            </div>
        </section>
    }
}

// Drag and the context menu are delegated document listeners in
// `card_menu_script`, reading the card's `data-task-id`/`data-from-column`
// — per-element handlers would die when a soft submit swaps the board in.
async fn render_card(
    cx: &Cx,
    card: TaskCard,
    today: Date,
    done_column: bool,
    column_id: &str,
    may_write: bool,
    all_columns: &[(String, String)],
    lang: Lang,
) -> Result {
    let blocks = card.blocks.len();
    let blocked_by = card.blocked_by.join(", ");
    let overdue = card.is_overdue(today);
    let deadline_parts = card.deadline_parts(today);
    let dated = deadline_parts.is_some();
    let deadline = match deadline_parts {
        Some(parts) => match parts.state {
            DeadlineState::Overdue => format!("{} · {}", parts.date, t(lang, Key::Overdue)),
            DeadlineState::Done => format!("{}{}", t(lang, Key::DonePrefix), parts.date),
            DeadlineState::OnTime => parts.date,
        },
        None => t(lang, Key::NoDeadline).to_string(),
    };
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
    view! {
        cx =>
        <a class="card" class:card-done=(done_column) href=(href.clone())
            draggable=(if may_write { "true" } else { "false" })
            data-task-id=(task_id.clone())
            data-from-column=(from_column.clone())
            data-menu-id=(menu_id.clone())
        >
            <div class="card-keys">
                <span class="card-key">(card.task_key.clone())</span>
                if blocks > 0 { <span class="card-blocks">(format!("{} {blocks}", t(lang, Key::Blocks)))</span> }
                if !blocked_by.is_empty() {
                    <span class="card-blocked-by">(format!("{} {blocked_by}", t(lang, Key::BlockedBy)))</span>
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
                        <span class="avatar avatar-none" role="img" aria-label=(t(lang, Key::NobodyAssignedAria)) title=(t(lang, Key::NobodyAssignedTitle))></span>
                    }
                </div>
            </div>
        </a>
        <div class="card-menu pop-panel" id=(menu_id.clone())>
            <div class="pop-list">
                <a class="pop-row" href=(href.clone())>(t(lang, Key::Open))</a>
                if may_write && !move_targets.is_empty() {
                    <div class="card-menu-move">
                        <button class="pop-row card-menu-move-trigger" type="button">(t(lang, Key::Move))</button>
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
                    <a class="pop-row card-menu-danger" href=(format!("/?task={}&confirm=delete", task_id))>(t(lang, Key::Delete))</a>
                }
            </div>
        </div>
    }
}

/// Opens a card's context menu at the cursor and closes whichever one was
/// open; a plain document click closes it again. Rendered once — every
/// card's `@contextmenu` calls into this same global, by the menu's id.
///
/// Also holds the board page's own `Escape` listener — now only the
/// datepicker panel and the card menu: viewer, delete confirm, open edit
/// popovers and the modal itself belong to `detail::escape_closes`, which
/// registers first (inline in the modal markup) and stops the key. The
/// topbar `.user-menu` is handled one script earlier, by
/// `layout::escape_script`, registered before this one so its blur wins
/// over anything below.
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
        document.addEventListener('click', closeCardMenus); \
        document.addEventListener('keydown', function (e) { \
            if (e.key !== 'Escape') { return; } \
            var datepick = document.querySelector('.datepick-pop > .edit-toggle:checked'); \
            if (datepick) { datepick.checked = false; e.stopImmediatePropagation(); return; } \
            var menu = document.querySelector('.card-menu-open'); \
            if (menu) { closeCardMenus(); e.stopImmediatePropagation(); return; } \
        }, true); \
        document.addEventListener('contextmenu', function (e) { \
            var card = e.target.closest ? e.target.closest('.card[data-menu-id]') : null; \
            if (!card) { return; } \
            e.preventDefault(); \
            window.__izlekOpenCardMenu(e, card.dataset.menuId); \
        }); \
        document.addEventListener('dragstart', function (e) { \
            var card = e.target.closest ? e.target.closest('.card[data-task-id]') : null; \
            if (!card) { return; } \
            e.dataTransfer.setData('izlek-task', card.dataset.taskId); \
            e.dataTransfer.setData('izlek-from', card.dataset.fromColumn); \
        }); \
        document.addEventListener('dragover', function (e) { \
            if (e.target.closest && e.target.closest('.board-columns')) { e.preventDefault(); } \
        }); \
        document.addEventListener('drop', function (e) { \
            var column = e.target.closest ? e.target.closest('.column-cards') : null; \
            if (!column) { return; } \
            e.preventDefault(); \
            var task = e.dataTransfer.getData('izlek-task'); \
            var from = e.dataTransfer.getData('izlek-from'); \
            if (task && window.__izlekPost) { \
                window.__izlekPost('/api/move_card', { task_id: task, from_column_id: from, to_column_id: column.id }); \
            } \
        })";
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
/// (if any) a create or move landed back here with. `file` opens the viewer
/// over that task's modal, the same nested query-param convention `confirm`
/// already uses.
#[query_params(error = redirect("/"))]
struct BoardQuery {
    task: Option<String>,
    file: Option<String>,
    refusal: Option<String>,
    on: Option<String>,
    sort: Option<String>,
    confirm: Option<String>,
    new: Option<String>,
}

/// The noun a sort key shows in the `<select>` — terse, no explainer.
fn sort_label(sort: &str, lang: Lang) -> &'static str {
    match sort {
        "created" => t(lang, Key::SortCreated),
        "title" => t(lang, Key::SortTitle),
        _ => t(lang, Key::SortDeadline),
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
    let lang = Lang::from_code(&user.language);
    let Some(view_data) = view_data else {
        return view! {
            cx =>
            <main class="scaffold-note"><p>(t(lang, Key::SomethingWentWrong))</p></main>
        };
    };
    let today = OffsetDateTime::now_utc().date();
    let overdue = view_data.overdue_count(today);
    let blocked = view_data.blocked_count();
    let may_write = user.role.can_write_tasks();
    let all_columns: Vec<(String, String)> =
        view_data.columns.iter().map(|column| (column.column.id.clone(), column.column.name.clone())).collect();
    let query = query_params::<BoardQuery>(cx)?;
    let open_task = query.task.clone();
    // `?task=X&new=1` together would render both modals at once — two
    // document-level datepicker listeners double-stepping the month nav — so
    // an open task wins and `new` is ignored.
    let open_new = may_write && query.new.is_some() && open_task.is_none();
    let sort = valid_sort(query.sort.as_deref()).to_string();
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
            (crate::layout::user_menu(cx, &user.display_name, &user.email, user.role, lang).await?)
        </header>
        <div class="filterbar">
            <div class="topbar-divider"></div>
            if may_write {
                <a class="primary new-task-open" href="/?new=1">(t(lang, Key::NewTask))</a>
            }
            <form class="field-box field-box-sort" method="get" action="/">
                <span class="field-text">(t(lang, Key::Sort))</span>
                <select class="status-select" name="sort" data-autosubmit="">
                    for key in SORT_KEYS {
                        <option value=(key) selected=(key == sort)>(sort_label(key, lang))</option>
                    }
                </select>
                <svg class="glyph" width="14" height="14" viewBox="0 0 16 16" fill="none"
                    stroke="currentColor" stroke-width="1.5" stroke-linecap="round"
                    stroke-linejoin="round" aria-hidden="true">
                    <path d="M4 6l4 4 4-4"></path>
                </svg>
            </form>
            <div class="spacer"></div>
            if overdue > 0 { <span class="chip chip-overdue">(format!("{overdue} {}", t(lang, Key::Overdue)))</span> }
            if blocked > 0 { <span class="chip chip-blocked">(format!("{blocked} {}", t(lang, Key::Blocked)))</span> }
        </div>
        if let Some(refusal) = &refusal {
            <p class="field-error">(refusal.message_in(lang))</p>
        }
        <main class="board-stage">
            <div class="board-columns">
                (board_columns(cx, sort.clone()).await?)
            </div>
        </main>
        if let Some(task_id) = &open_task {
            (crate::detail::task_modal(cx, task_id, query.confirm.as_deref() == Some("delete")).await?)
            if let Some(file_id) = &query.file {
                (crate::detail::file_viewer_modal(cx, task_id, file_id).await?)
            }
        }
        if open_new {
            (crate::detail::new_task_modal(cx, &all_columns, lang).await?)
        }
        (crate::dropdown::dropdown_script(cx).await?)
        (crate::layout::escape_script(cx).await?)
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
