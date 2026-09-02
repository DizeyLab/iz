//! "What changed for me": the events a signed-in person has a stake in.
//!
//! Every role gets it — a Viewer's feed is simply emptier, not hidden. Two
//! things put a line on someone's feed: it happened on a task they watch
//! (making a task, being assigned, commenting each start a watch; an
//! unassign ends it), or it names them. Nobody's own actions come back to
//! them — the store's feed query leaves those off. The watch and marker
//! mechanics live in the store (`feed_for_user`, `count_feed_unseen`,
//! `mark_feed_seen`); this page only reads and renders.
//!
//! Visiting is reading: the page marks the feed seen after fetching its
//! rows, so the badge on the next page anywhere — and on this one, which
//! renders after the mark — reads zero. New events announce on the live
//! channel like every other write, and the shell's refresh morph brings
//! both this page and the topbar badge back up to date.

use time::OffsetDateTime;
use topcoat::Result;
use topcoat::context::Cx;
use topcoat::router::{error::not_found, page};
use topcoat::view::view;


use crate::i18n::{Key, Lang, t};
use crate::server::{accounts, require_user};

/// How many lines one page draws, the same cap the logs page reads by: a
/// read a person can finish, panned inside the shell, never a document
/// growing past the viewport.
const LIMIT: u32 = 50;

/// One rendered feed line: the strings the view stamps in, decided here so
/// the template stays a list of facts rather than a chain of lookups.
struct FeedRow {
    task_label: String,
    href: String,
    actor: String,
    sentence: String,
    when: String,
}

/// The person's feed. `/feed`.
#[page("/feed")]
async fn feed_page(cx: &Cx) -> Result {
    let user = match require_user(cx).await {
        Ok(user) => user,
        Err(_) => return Err(not_found().into()),
    };
    let lang = Lang::from_code(&user.language);
    let store = accounts(cx).store().clone();
    let lines = store.feed_for_user(&user.id, LIMIT).await?;
    // The visit is the marker, taken after the rows are read: the badge on
    // the next page anywhere reads zero, and this page's own topbar —
    // rendered after the mark — agrees with it.
    store
        .mark_feed_seen(&user.id, OffsetDateTime::now_utc())
        .await?;
    let zone = izlek_core::detail::parse_zone(&user.timezone);

    let rows: Vec<FeedRow> = lines
        .iter()
        .map(|line| FeedRow {
            task_label: match (&line.task_key, &line.title) {
                (Some(key), Some(title)) => format!("{key} · {title}"),
                (Some(key), None) => key.clone(),
                _ => line.title.clone().unwrap_or_default(),
            },
            href: format!("/?task={}", line.task_id.as_deref().unwrap_or_default()),
            actor: line
                .actor_name
                .clone()
                .unwrap_or_else(|| t(lang, Key::TheSystem).to_string()),
            sentence: crate::logs::activity_sentence(&line.kind, &line.detail, lang),
            when: izlek_core::detail::moment_label_in(line.at, zone),
        })
        .collect();

    view! {
        cx =>
        <header class="topbar">
            (crate::layout::mark(cx).await?)
            (crate::layout::topbar_nav(cx, crate::layout::NavPage::Feed, user.role, lang).await?)
            <div class="spacer"></div>
            (crate::layout::user_menu(cx, &crate::detail::Me::from(&user), lang).await?)
        </header>

        <main class="feed-shell">
            <section class="panel">
                <div class="panel-head">
                    <h2 class="panel-title">(t(lang, Key::NavFeed))</h2>
                </div>
                if rows.is_empty() {
                    <p class="feed-empty">(t(lang, Key::FeedEmpty))</p>
                } else {
                    <ol class="feed-list">
                        for row in rows {
                            <li class="feed-row">
                                <a class="feed-task" href=(row.href)>(row.task_label)</a>
                                <span class="feed-line">
                                    <span class="feed-actor">(row.actor)</span>
                                    <span class="feed-sentence">(row.sentence)</span>
                                </span>
                                <span class="feed-when">(row.when)</span>
                            </li>
                        }
                    </ol>
                }
            </section>
        </main>
    }
}
