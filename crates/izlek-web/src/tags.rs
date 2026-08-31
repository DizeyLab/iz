//! Task tags, ported from `izlek-web/src/rules.rs` onto topcoat's
//! server-rendered pages.
//!
//! The screen is the admin's, and so is every call behind it: a tag renames
//! what every member sees on the board and in the task modal, so writing one
//! is not something a Member may do even if they reach the endpoint directly.
//! The chip in the artboard says "Admin only"; the guard is in the handlers.
//!
//! There is no client-side signal here to hold which row is being edited —
//! every topcoat page is rendered fresh, server-side, on every request. Which
//! tag (if any) is open for editing rides `?edit=<tag_id>` on `/tags`
//! itself: the row renders as the rename form server-side instead of the
//! display row, no script required.
//!
//! The board's default tag ships no delete control: the store refuses the
//! delete anyway (every task falls back to it), and a control that cannot
//! act is not drawn.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use topcoat::Result;
use topcoat::context::Cx;
use topcoat::router::content::{Form, Json};
use topcoat::router::request::headers;
use topcoat::router::{HeaderName, StatusCode, header, page, query_params, route};
use topcoat::view::view;

use izlek_core::store::{Store, Tag, User};

use crate::i18n::{Key, Lang, t};
use crate::server::{Refusal, accounts, refusal_of, require_admin};

/// One tag as the screen reads it: a name, an order and whether it is the
/// default every task wears when nothing else is chosen.
#[derive(Clone, Debug, Serialize)]
struct TagLine {
    id: String,
    name: String,
    position: i64,
    is_default: bool,
    /// Cards wearing this tag. A tag with any is not deletable, and the row
    /// says how many rather than leaving the admin to guess why.
    tasks: u32,
}

/// Every tag on the workspace's own board, in the admin's hand-set order.
async fn tags_of(
    store: &Arc<dyn Store>,
    user: &User,
) -> std::result::Result<Vec<TagLine>, Refusal> {
    let Some(board) = store
        .board(&user.workspace_id)
        .await
        .map_err(|_| Refusal::Unavailable)?
    else {
        return Err(Refusal::Unavailable);
    };
    let counts = store
        .tag_task_counts(&board.id)
        .await
        .map_err(|_| Refusal::Unavailable)?;
    Ok(store
        .tags(&board.id)
        .await
        .map_err(|_| Refusal::Unavailable)?
        .into_iter()
        .map(|tag| TagLine {
            tasks: counts
                .iter()
                .find(|(id, _)| id == &tag.id)
                .map(|(_, count)| *count)
                .unwrap_or(0),
            id: tag.id,
            name: tag.name,
            position: tag.position,
            is_default: tag.is_default,
        })
        .collect())
}

/// The admin's store, once the tag id has been shown to name a tag on this
/// workspace's own board.
///
/// A tag id is opaque and arrives from the browser, so "not yours" and "not a
/// tag" are the same answer: neither tells the caller whether the id exists
/// somewhere else.
async fn tag_of_this_workspace(
    cx: &Cx,
    tag_id: &str,
) -> std::result::Result<(Arc<dyn Store>, User, Tag), Refusal> {
    let user = require_admin(cx).await?;
    let store = accounts(cx).store().clone();
    let board = store
        .board(&user.workspace_id)
        .await
        .map_err(|_| Refusal::Unavailable)?
        .ok_or(Refusal::Unavailable)?;
    let tag = store
        .tags(&board.id)
        .await
        .map_err(|_| Refusal::Unavailable)?
        .into_iter()
        .find(|tag| tag.id == tag_id)
        .ok_or(Refusal::NotFound)?;
    Ok((store, user, tag))
}

/// The page a browser without script is sent back to on a 303: the page the
/// form was posted from, or home when there is no `Referer` to read.
fn back_to(cx: &Cx) -> String {
    headers(cx)
        .get(header::REFERER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("/")
        .to_string()
}

/// A 303 to [`back_to`], carrying `refusal` as the body for
/// `crate::server::carry_refusal_on_redirect` to read and copy onto the query.
type Redirect = Result<(StatusCode, [(HeaderName, String); 1], Json<Option<Refusal>>)>;

fn redirect(cx: &Cx, refusal: Option<Refusal>) -> Redirect {
    Ok((
        StatusCode::SEE_OTHER,
        [(header::LOCATION, back_to(cx))],
        Json(refusal),
    ))
}

#[derive(Deserialize)]
struct CreateTagForm {
    name: String,
}

/// Writes one tag. A duplicate name is the store's `Conflict`, which lands
/// here like every other store error: the admin sees "something went wrong"
/// and the list still shows the tag they collide with.
#[route(POST "/api/create_tag")]
async fn create_tag(cx: &Cx, Form(input): Form<CreateTagForm>) -> Redirect {
    let user = match require_admin(cx).await {
        Ok(user) => user,
        Err(refusal) => return redirect(cx, Some(refusal)),
    };
    let name = input.name.trim().to_string();
    if name.is_empty() {
        return redirect(cx, Some(Refusal::EmptyTag));
    }
    let store = accounts(cx).store().clone();
    let board = match store.board(&user.workspace_id).await {
        Ok(Some(board)) => board,
        _ => return redirect(cx, Some(Refusal::Unavailable)),
    };
    if store
        .create_tag(&board.id, &name, time::OffsetDateTime::now_utc())
        .await
        .is_err()
    {
        return redirect(cx, Some(Refusal::Unavailable));
    }
    redirect(cx, None)
}

#[derive(Deserialize)]
struct RenameTagForm {
    tag_id: String,
    name: String,
}

/// Rewrites a tag's name in place, guarded exactly like the rest — the tag
/// itself has to be this workspace's, or an id from elsewhere could be
/// renamed by an admin who never owned it.
#[route(POST "/api/rename_tag")]
async fn rename_tag(cx: &Cx, Form(input): Form<RenameTagForm>) -> Redirect {
    let (store, _, _) = match tag_of_this_workspace(cx, &input.tag_id).await {
        Ok(triple) => triple,
        Err(refusal) => return redirect(cx, Some(refusal)),
    };
    let name = input.name.trim().to_string();
    if name.is_empty() {
        return redirect(cx, Some(Refusal::EmptyTag));
    }
    if store.rename_tag(&input.tag_id, &name).await.is_err() {
        return redirect(cx, Some(Refusal::Unavailable));
    }
    redirect(cx, None)
}

#[derive(Deserialize)]
struct DeleteTagForm {
    tag_id: String,
}

/// Removes a tag. The store refuses two of them: the default — every task
/// without a project of its own wears it — and any tag that still has cards,
/// which comes back named so the admin reads why rather than "something went
/// wrong". Neither row ships a delete control in the first place; this is
/// the same rule where it is actually enforced.
#[route(POST "/api/delete_tag")]
async fn delete_tag(cx: &Cx, Form(input): Form<DeleteTagForm>) -> Redirect {
    let (store, _, _) = match tag_of_this_workspace(cx, &input.tag_id).await {
        Ok(triple) => triple,
        Err(refusal) => return redirect(cx, Some(refusal)),
    };
    match store.delete_tag(&input.tag_id).await {
        Ok(()) => redirect(cx, None),
        Err(izlek_core::store::StoreError::Conflict("tag_in_use")) => {
            redirect(cx, Some(Refusal::TagInUse))
        }
        Err(_) => redirect(cx, Some(Refusal::Unavailable)),
    }
}

#[derive(Deserialize)]
struct MoveTagForm {
    tag_id: String,
    direction: String,
}

/// Steps a tag up or down the board's hand-set order. A move off either end
/// is the store's no-op, not an error; a direction that is neither word is a
/// form gone wrong, and does nothing.
#[route(POST "/api/move_tag")]
async fn move_tag(cx: &Cx, Form(input): Form<MoveTagForm>) -> Redirect {
    let (store, _, _) = match tag_of_this_workspace(cx, &input.tag_id).await {
        Ok(triple) => triple,
        Err(refusal) => return redirect(cx, Some(refusal)),
    };
    let up = match input.direction.as_str() {
        "up" => true,
        "down" => false,
        _ => return redirect(cx, None),
    };
    if store.move_tag(&input.tag_id, up).await.is_err() {
        return redirect(cx, Some(Refusal::Unavailable));
    }
    redirect(cx, None)
}

/// Which tag (if any) `/tags` renders open for editing.
#[query_params(error = redirect("/tags"))]
struct TagsQuery {
    edit: Option<String>,
}

/// The "New tag" control and the form inside it. A `<details>` rather than
/// anything script-driven, so a browser with no script can still open it and
/// post the form.
async fn composer(cx: &Cx, refusal: Option<&Refusal>, lang: Lang) -> Result {
    view! {
        cx =>
        <details class="tag-new">
            <summary class="tag-new-open">(t(lang, Key::NewTag))</summary>
            <form class="tag-form" method="post" action="/api/create_tag">
                <label class="tag-field">
                    <span class="field-label">(t(lang, Key::NameLabel))</span>
                    <input class="field-input" type="text" name="name" maxlength="60" required="">
                </label>
                <div class="panel-foot">
                    if let Some(refusal) = refusal {
                        <span class="field-error">(refusal.message_in(lang))</span>
                    }
                    <button class="primary" type="submit">(t(lang, Key::AddTag))</button>
                </div>
            </form>
        </details>
    }
}

/// One tag's row: the rename form in place of it when `editing`, its display
/// otherwise. The default tag draws no delete form, and neither does a tag
/// with cards on it — the store would refuse both, and a control that cannot
/// act is not drawn. The card count sits on the row, so a missing delete is
/// a number the admin can already see rather than a control that vanished.
async fn tag_row(cx: &Cx, tag: &TagLine, editing: bool, refusal: Option<&Refusal>, lang: Lang) -> Result {
    if editing {
        return view! {
            cx =>
            <div class="tag-row">
                <form class="tag-form" method="post" action="/api/rename_tag">
                    <input type="hidden" name="tag_id" value=(tag.id.clone())>
                    <label class="tag-field">
                        <span class="field-label">(t(lang, Key::NameLabel))</span>
                        <input class="field-input" type="text" name="name" maxlength="60" required="" value=(tag.name.clone())>
                    </label>
                    <div class="panel-foot">
                        if let Some(refusal) = refusal {
                            <span class="field-error">(refusal.message_in(lang))</span>
                        }
                        <a class="quiet" href="/tags">(t(lang, Key::Cancel))</a>
                        <button class="primary" type="submit">(t(lang, Key::SaveTag))</button>
                    </div>
                </form>
            </div>
        };
    }

    view! {
        cx =>
        <div class="tag-row">
            <span class="tag-name">(tag.name.clone())</span>
            <span class="tag-count">(tag.tasks)</span>

            <a class="quiet" href=(format!("/tags?edit={}", tag.id)) title=(t(lang, Key::EditThisTag))>(t(lang, Key::EditLabel))</a>

            <form method="post" action="/api/move_tag">
                <input type="hidden" name="tag_id" value=(tag.id.clone())>
                <input type="hidden" name="direction" value="up">
                <button class="quiet" type="submit" title=(t(lang, Key::MoveTagUp)) aria-label=(t(lang, Key::MoveTagUp))>("\u{2191}")</button>
            </form>
            <form method="post" action="/api/move_tag">
                <input type="hidden" name="tag_id" value=(tag.id.clone())>
                <input type="hidden" name="direction" value="down">
                <button class="quiet" type="submit" title=(t(lang, Key::MoveTagDown)) aria-label=(t(lang, Key::MoveTagDown))>("\u{2193}")</button>
            </form>

            if !tag.is_default && tag.tasks == 0 {
                <form method="post" action="/api/delete_tag">
                    <input type="hidden" name="tag_id" value=(tag.id.clone())>
                    <button class="quiet quiet-danger" type="submit" title=(t(lang, Key::DeleteThisTag))>(t(lang, Key::Delete))</button>
                </form>
            }
        </div>
    }
}

#[page("/tags")]
async fn tags_page(cx: &Cx) -> Result {
    let user = match require_admin(cx).await {
        Ok(user) => user,
        Err(refusal) => {
            return view! {
                <main class="scaffold-note">
                    <p>(refusal.message())</p>
                    <p><a href="/">(t(Lang::En, Key::BackToBoard))</a></p>
                </main>
            };
        }
    };
    let lang = Lang::from_code(&user.language);
    let store = accounts(cx).store().clone();
    let tags = match tags_of(&store, &user).await {
        Ok(tags) => tags,
        Err(refusal) => {
            return view! {
                <main class="scaffold-note">
                    <p>(refusal.message_in(lang))</p>
                    <p><a href="/">(t(lang, Key::BackToBoard))</a></p>
                </main>
            };
        }
    };
    let edit_id = query_params::<TagsQuery>(cx)?.edit.clone();
    let create_refusal = refusal_of(cx, "create_tag");
    let rename_refusal = refusal_of(cx, "rename_tag");

    view! {
        <header class="topbar">
            (crate::layout::mark(cx).await?)
            (crate::layout::topbar_nav(cx, crate::layout::NavPage::Tags, user.role, lang).await?)
            <div class="spacer"></div>
            (crate::layout::user_menu(cx, &crate::detail::Me::from(&user), lang).await?)
        </header>

        <div class="settings-shell">
            <main class="settings-stage">
                <div class="settings-head">
                    <h1 class="settings-title">(t(lang, Key::NavTags))</h1>
                    <span class="chip chip-admin">(t(lang, Key::AdminOnly))</span>
                </div>

                (composer(cx, create_refusal.as_ref(), lang).await?)

                <div class="tag-list">
                    for tag in &tags {
                        (tag_row(cx, tag, edit_id.as_deref() == Some(tag.id.as_str()), rename_refusal.as_ref(), lang).await?)
                    }
                </div>
            </main>
            (crate::dropdown::dropdown_script(cx).await?)
            (crate::layout::escape_script(cx).await?)
        </div>
    }
}
