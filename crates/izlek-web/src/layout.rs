//! The document shell every page renders inside, ported from
//! `izlek-web/src/app.rs`'s `shell`/`App`/`NotFound`.

use topcoat::{
    Result,
    asset::{Asset, asset},
    context::Cx,
    router::{
        StatusCode,
        error::{NotFoundError, not_found},
        layout, page,
    },
    view::{Unescaped, view},
};

use izlek_core::board::Person;

use crate::i18n::{Key, Lang, t};
use crate::server::current_user;

/// A person as a circle (or, on the Instrument skin, a square) — the initials
/// avatar when there is no photo, an `<img>` reading `/photo/{id}` otherwise.
/// Shared by the board, the modal and every topbar user menu.
pub(crate) async fn avatar(cx: &Cx, person: &Person, extra: &str) -> Result {
    let tone = person
        .id
        .bytes()
        .fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32))
        % 5;
    let class = format!("avatar avatar-tone-{tone} {extra}");
    let name = person.display_name.clone();
    if person.has_photo {
        let src = format!("/photo/{}", person.id);
        view! {
            cx =>
            <img class=(class) src=(src) alt=(name.clone()) data-name=(name)>
        }
    } else {
        let initials = person.initials();
        view! {
            cx =>
            <span class=(class) data-name=(name)>(initials)</span>
        }
    }
}

/// The topbar's signed-in identity: the display name, opening on hover or
/// focus onto details (name, address, role), settings and sign-out. Shared
/// by every signed-in page's topbar (board, settings, mail rules, logs).
pub async fn user_menu(cx: &Cx, me: &crate::detail::Me, lang: Lang) -> Result {
    let role_key = match me.role {
        izlek_core::Role::Admin => Key::RoleAdminOption,
        izlek_core::Role::Member => Key::RoleMemberOption,
        izlek_core::Role::Viewer => Key::RoleViewerOption,
    };
    let person = izlek_core::board::Person {
        id: me.id.clone(),
        display_name: me.display_name.clone(),
        has_photo: me.has_photo,
    };
    view! {
        cx =>
        <div class="user-menu">
            <button type="button" class="user-menu-trigger">
                (avatar(cx, &person, "").await?)
                (me.display_name.clone())
            </button>
            <div class="user-menu-panel">
                <div class="user-menu-name">(me.display_name.clone())</div>
                <div class="user-menu-email">(me.email.clone())</div>
                <div class="user-menu-role">(t(lang, role_key))</div>
                <div class="user-menu-divider"></div>
                <a class="user-menu-item" href="/settings">(t(lang, Key::Settings))</a>
                <form class="user-menu-item-form" method="post" action="/api/sign_out" data-hard="">
                    <button class="user-menu-item" type="submit">(t(lang, Key::SignOut))</button>
                </form>
            </div>
        </div>
    }
}

/// The four signed-in pages, as the topbar nav links between them.
#[derive(Clone, Copy, PartialEq)]
pub enum NavPage {
    Board,
    Rules,
    Logs,
    Settings,
}

impl NavPage {
    const ALL: [Self; 4] = [Self::Board, Self::Rules, Self::Logs, Self::Settings];

    fn href(self) -> &'static str {
        match self {
            Self::Board => "/",
            Self::Rules => "/rules",
            Self::Logs => "/logs",
            Self::Settings => "/settings",
        }
    }

    fn label(self) -> Key {
        match self {
            Self::Board => Key::NavBoard,
            Self::Rules => Key::NavMailRules,
            Self::Logs => Key::NavLogs,
            Self::Settings => Key::NavSettings,
        }
    }
}

/// The topbar's page nav, shared by every signed-in page: the four pages
/// with the current one marked. Plain `<a>`s — the soft-nav forwarder
/// swaps them like any same-origin link, so `data-hard` stays off.
pub async fn topbar_nav(cx: &Cx, active: NavPage, lang: Lang) -> Result {
    view! {
        cx =>
        <nav class="topbar-nav-links">
            for page in NavPage::ALL {
                <a
                    class=(if page == active { "topbar-nav topbar-nav-on" } else { "topbar-nav" })
                    href=(page.href())
                >
                    (t(lang, page.label()))
                </a>
            }
        </nav>
    }
}

/// The page-swap machinery that keeps every mutation on the same document.
///
/// Every `/api/*` mutation is a plain form post answered with a 303 — sound
/// without script, but a full navigation repaints the whole page for a
/// one-field change. This script, emitted first in `<body>` on every page,
/// intercepts those submits (capture-phase, so `requestSubmit()` from any
/// auto-submit control lands here too), replays them over `fetch`, and swaps
/// the redirected page's `<body>` children in place — same bytes the hard
/// navigation would have shown, no reload. Refusal banners and saved chips
/// arrive with the swap because the redirect URL's query params rendered
/// them server-side; `history.replaceState` keeps the address bar honest.
///
/// Swapped-in `<script>` nodes are inert (parser-inserted only), so
/// `swap()` re-creates each one as a fresh element, which runs it; every
/// emitted script carries a one-shot `window.__izlek*` guard, so that
/// re-execution never stacks duplicate document-level listeners. There
/// is no topcoat re-hydrate entry, so per-element behavior is re-run by
/// the `izlek:wire` event (`dropdown.rs`, the audio player) over the new
/// nodes.
///
/// Forms that must really navigate (sign-in/out, claim, redeem: the session
/// cookie and the whole page identity change) opt out with `data-hard`.
/// Overlay closes never hit the server at all: the board is already
/// rendered under the modal, so `__izlekCloseModal`/`__izlekCloseViewer`
/// drop the overlay's DOM and rewrite the URL.
///
/// The click-forwarder at the end is the repo-wide fix for the dead-zone
/// class the member-role select first showed: any click inside a
/// `.field-box`/`.status-form`/`.member-role` that misses the `.dd-trigger`
/// (label, chevron glyph, box padding) is forwarded to the trigger.
///
/// Ordinary same-origin links get the same treatment: a delegated capture
/// click listener — registered after the close/scrim one, and skipped when
/// that one already `preventDefault`ed — fetches the href and swaps it in,
/// then `history.pushState`s, so board cards and file chips no longer
/// reload the page. Raw `/files/` byte routes, `download`/`target`/
/// `data-hard` links and modified clicks (ctrl/meta/shift/alt, non-left
/// button) stay browser-native. `popstate` replays the same fetch without
/// pushing. Fresh-URL navigations pass `swap`'s third argument to scroll
/// to top; in-place swaps (form posts, back) keep the scroll position.
/// `wire()` also pins every `.comment-list` to its bottom, on initial
/// load (DOMContentLoaded) included.
pub async fn soft_nav_script(cx: &Cx) -> Result {
    const JS: &str = "\
        (function () { \
            if (window.__izlekSoft) { return; } \
            window.__izlekSoft = true; \
            function wire() { \
                document.querySelectorAll('.comment-list').forEach(function (list) { list.scrollTop = list.scrollHeight; }); \
                document.dispatchEvent(new Event('izlek:wire')); \
            } \
            function swap(html, url, fresh) { \
                var doc = new DOMParser().parseFromString(html, 'text/html'); \
                var x = window.scrollX, y = window.scrollY; \
                document.body.replaceChildren(); \
                while (doc.body.firstChild) { document.body.appendChild(doc.body.firstChild); } \
                document.body.querySelectorAll('script').forEach(function (old) { \
                    var live = document.createElement('script'); \
                    if (old.hasAttribute('src')) { live.setAttribute('src', old.getAttribute('src')); } \
                    live.textContent = old.textContent; \
                    old.replaceWith(live); \
                }); \
                var root = doc.documentElement; \
                if (root.getAttribute('lang')) { document.documentElement.setAttribute('lang', root.getAttribute('lang')); } \
                if (root.hasAttribute('data-theme')) { document.documentElement.setAttribute('data-theme', root.getAttribute('data-theme')); } \
                else { document.documentElement.removeAttribute('data-theme'); } \
                if (root.hasAttribute('data-ui')) { document.documentElement.setAttribute('data-ui', root.getAttribute('data-ui')); } \
                else { document.documentElement.removeAttribute('data-ui'); } \
                if (url) { history.replaceState(null, '', url); } \
                window.scrollTo(fresh ? 0 : x, fresh ? 0 : y); \
                wire(); \
            } \
            function sweepPanels() { \
                document.querySelectorAll('.dd-panel').forEach(function (panel) { \
                    if (panel.__ddTrigger && !document.contains(panel.__ddTrigger)) { panel.remove(); } \
                }); \
            } \
            window.__izlekGo = function (url) { \
                fetch(url).then( \
                    function (r) { return r.text().then(function (t) { swap(t, r.url); }); }, \
                    function () { window.location.href = url; } \
                ); \
            }; \
            window.__izlekPost = function (action, fields) { \
                fetch(action, { method: 'POST', body: new URLSearchParams(fields) }).then( \
                    function (r) { return r.text().then(function (t) { swap(t, r.url); }); }, \
                    function () {} \
                ); \
            }; \
            window.__izlekCloseViewer = function () { \
                var scrim = document.querySelector('.viewer-scrim'); \
                if (!scrim) { return false; } \
                var back = scrim.querySelector('.viewer-close').getAttribute('href'); \
                scrim.remove(); \
                sweepPanels(); \
                history.replaceState(null, '', back); \
                var modal = document.querySelector('.modal'); \
                if (modal) { modal.focus(); } \
                return true; \
            }; \
            window.__izlekCloseModal = function () { \
                var scrims = document.querySelectorAll('.modal-scrim'); \
                if (!scrims.length) { return false; } \
                scrims.forEach(function (el) { el.remove(); }); \
                sweepPanels(); \
                var u = new URL(window.location.href); \
                ['task', 'file', 'confirm', 'new', 'refusal', 'on'].forEach(function (k) { u.searchParams.delete(k); }); \
                var q = u.searchParams.toString(); \
                history.replaceState(null, '', u.pathname + (q ? '?' + q : '')); \
                return true; \
            }; \
            document.addEventListener('submit', function (e) { \
                var form = e.target; \
                if (!form || form.hasAttribute('data-hard')) { return; } \
                var method = (form.getAttribute('method') || 'get').toLowerCase(); \
                if (method !== 'post') { \
                    e.preventDefault(); \
                    var q = new URLSearchParams(new FormData(form)).toString(); \
                    window.__izlekGo((form.getAttribute('action') || window.location.pathname) + (q ? '?' + q : '')); \
                    return; \
                } \
                e.preventDefault(); \
                var data = e.submitter ? new FormData(form, e.submitter) : new FormData(form); \
                var multipart = (form.getAttribute('enctype') || '').indexOf('multipart') !== -1; \
                fetch(form.getAttribute('action'), { method: 'POST', body: multipart ? data : new URLSearchParams(data) }).then( \
                    function (r) { return r.text().then(function (t) { swap(t, r.url); }); }, \
                    function () { form.submit(); } \
                ); \
            }, true); \
            document.addEventListener('change', function (e) { \
                var control = e.target; \
                if (control.classList && control.classList.contains('file-upload-input')) { \
                    var name = control.closest('label').querySelector('.file-upload-name'); \
                    if (control.files && control.files[0]) { name.textContent = control.files[0].name; } \
                    control.form.requestSubmit(); \
                    return; \
                } \
                if (control.hasAttribute && control.hasAttribute('data-autosubmit')) { control.form.requestSubmit(); } \
            }); \
            document.addEventListener('click', function (e) { \
                var link = e.target.closest ? e.target.closest('a') : null; \
                if (link) { \
                    if (link.classList.contains('viewer-close') || (link.classList.contains('detail-close') && link.closest('.viewer-scrim'))) { \
                        e.preventDefault(); \
                        window.__izlekCloseViewer(); \
                        return; \
                    } \
                    if (link.getAttribute('href') === '/' && link.closest('.modal-scrim')) { \
                        e.preventDefault(); \
                        window.__izlekCloseModal(); \
                        return; \
                    } \
                } \
                if (e.target.classList && e.target.classList.contains('modal-scrim')) { \
                    if (e.target.classList.contains('viewer-scrim')) { window.__izlekCloseViewer(); } \
                    else { window.__izlekCloseModal(); } \
                } \
            }, true); \
            window.addEventListener('popstate', function () { window.__izlekGo(window.location.href); }); \
            document.addEventListener('click', function (e) { \
                if (e.defaultPrevented || e.button !== 0 || e.ctrlKey || e.metaKey || e.shiftKey || e.altKey) { return; } \
                var link = e.target.closest ? e.target.closest('a') : null; \
                if (!link || link.hasAttribute('download') || link.hasAttribute('target') || link.hasAttribute('data-hard')) { return; } \
                var href = link.getAttribute('href'); \
                if (!href || href.charAt(0) !== '/' || href.indexOf('/files/') === 0) { return; } \
                e.preventDefault(); \
                fetch(href).then( \
                    function (r) { return r.text().then(function (t) { swap(t, null, true); history.pushState(null, '', r.url); }); }, \
                    function () { window.location.href = href; } \
                ); \
            }, true); \
            document.addEventListener('click', function (e) { \
                if (e.target.closest && e.target.closest('.dd-trigger, .dd-panel, button, a, input, select, textarea')) { return; } \
                var box = e.target.closest ? e.target.closest('.field-box, .status-form, .member-role') : null; \
                if (!box) { return; } \
                var trigger = box.querySelector('.dd-trigger'); \
                if (trigger) { e.stopImmediatePropagation(); trigger.click(); } \
            }); \
            if (document.readyState === 'loading') { document.addEventListener('DOMContentLoaded', wire); } else { wire(); } \
        })();";
    view! { cx => <script>(Unescaped::new_unchecked(JS))</script> }
}

/// The one document-level capture `keydown` listener for `Escape`, emitted
/// once from `root_layout` before every page's own scripts. Owners install
/// no listeners of their own; their scripts call
/// `window.__izlekEsc.register(priority, fn)` and on `Escape` the manager
/// runs the resolvers highest-priority first — the first to return true
/// closes its surface and the manager stops the key. Every consuming press
/// gets `preventDefault()`: a real Escape's browser default is stop-loading
/// (it once cancelled the navigation these branches used to start; synthetic
/// keys carry no default, so only a live keyboard ever showed it).
///
/// The table below is the whole precedence order, topmost first; a resolver
/// whose surface is absent returns false so lower layers get the press. It
/// reproduces what the four former listeners' registration order produced
/// per page (the modal's inline `escape_closes` registered before the board
/// page's trailing scripts, `dropdown_script` before `escape_script` on
/// settings/rules, `escape_script` before `card_menu_script` on the board):
///
/// | prio | closes | registered by |
/// |------|--------|---------------|
/// | 95 | the open dropdown panel (refocusing its trigger) | `dropdown.rs`'s `dropdown_script` |
/// | 90 | the viewer, then the delete confirm, then non-datepick edit popovers, then the task modal | `detail.rs`'s `escape_closes` |
/// | 40 | the topbar `.user-menu` (pin+blur), then the rules composer, then a rules edit row (back to `/rules`) | `layout.rs`'s `escape_script` |
/// | 20 | the datepicker popover | `board.rs`'s `card_menu_script` |
/// | 10 | the card context menu | `board.rs`'s `card_menu_script` |
pub async fn escape_manager_script(cx: &Cx) -> Result {
    use topcoat::view::Unescaped;
    const JS: &str = "\
        (function () { \
        if (window.__izlekEsc) { return; } \
        window.__izlekEsc = { \
            resolvers: [], \
            register: function (priority, fn) { \
                window.__izlekEsc.resolvers.push({ priority: priority, fn: fn }); \
                window.__izlekEsc.resolvers.sort(function (a, b) { return b.priority - a.priority; }); \
            } \
        }; \
        document.addEventListener('keydown', function (e) { \
            if (e.key !== 'Escape') { return; } \
            for (var i = 0; i < window.__izlekEsc.resolvers.length; i++) { \
                if (window.__izlekEsc.resolvers[i].fn(e)) { \
                    e.preventDefault(); \
                    e.stopImmediatePropagation(); \
                    return; \
                } \
            } \
        }, true); \
        })();";
    view! { cx => <script>(Unescaped::new_unchecked(JS))</script> }
}
/// Registers the topbar/rules `Escape` resolvers on `window.__izlekEsc`
/// (priority 40 — the table is on `escape_manager_script`): the topbar
/// `.user-menu` panel — hover-open included, pinned shut by a
/// `user-menu-esc` class that a `mouseenter` inside the menu clears — then
/// a `.rule-new` composer left open on the rules page, then a rules edit
/// row (navigating back to `/rules`). Never touches
/// `details.confirm-details`; that flow stays `detail.rs`'s.
pub async fn escape_script(cx: &Cx) -> Result {
    const JS: &str = "\
        (function () { \
        if (window.__izlekEscTop) { return; } \
        window.__izlekEscTop = true; \
        window.__izlekEsc.register(40, function () { \
            var menu = document.querySelector('.user-menu'); \
            if (menu && (menu.matches(':hover') || menu.contains(document.activeElement))) { \
                menu.classList.add('user-menu-esc'); \
                var focused = document.activeElement; \
                if (focused && focused.closest('.user-menu')) { focused.blur(); } \
                return true; \
            } \
            var composer = document.querySelector('details.rule-new[open]'); \
            if (composer) { composer.removeAttribute('open'); return true; } \
            if (document.querySelector('.rule-new-body[action=\"/api/update_rule\"]')) { \
                if (window.__izlekGo) { window.__izlekGo('/rules'); } else { window.location.href = '/rules'; } \
                return true; \
            } \
            return false; \
        }); \
        document.addEventListener('mouseenter', function (e) { \
            var menu = e.target.closest ? e.target.closest('.user-menu') : null; \
            if (menu) { menu.classList.remove('user-menu-esc'); } \
        }, true); \
        })();";
    view! { cx => <script>(Unescaped::new_unchecked(JS))</script> }
}

/// Every path that matches no page raises a `NotFoundError`, so it renders
/// through `root_layout`'s catch below rather than the router's bare default.
/// `/` itself is served by `landing` above and never reaches this route.
#[page("/{*path}")]
async fn missing() -> Result {
    Err(not_found().into())
}

/// `style/main.scss`, compiled by `build.rs` into `assets/main.css`.
const STYLE: Asset = asset!("assets/main.css");

#[layout("/")]
async fn root_layout(cx: &Cx, slot: Result) -> Result {
    // Pages with no session (auth screens) render light and English; both are
    // only set when the request's own user has one to read.
    let asking = match current_user(cx).await {
        Ok(user) => user.as_ref(),
        Err(_) => None,
    };
    let dark = asking.is_some_and(|user| user.theme == "dark");
    let ui = asking.map_or("instrument", |user| user.ui.as_str());
    let lang = asking.map_or(Lang::En, |user| Lang::from_code(&user.language));

    let content = match slot {
        Err(error) if error.downcast_ref::<NotFoundError>().is_some() => view! {
            cx =>
            (StatusCode::NOT_FOUND)
            <main class="scaffold-note">
                <p>(t(lang, Key::NothingAtThisAddress))</p>
            </main>
        },
        content => content,
    }?;

    view! {
        <!DOCTYPE html>
        <html lang=(lang.code()) data-theme=(dark.then_some("dark")) data-ui=(ui)>
            <head>
                <meta charset="utf-8">
                <meta name="viewport" content="width=device-width, initial-scale=1">
                <link rel="preconnect" href="https://fonts.googleapis.com">
                <link rel="preconnect" href="https://fonts.gstatic.com" crossorigin="">
                <link
                    rel="stylesheet"
                    href="https://fonts.googleapis.com/css2?family=IBM+Plex+Mono:wght@400;500&family=Newsreader:ital,wght@0,400;0,600;1,400;1,600&display=swap"
                >
                <title>"Izlek"</title>
                <link rel="stylesheet" href=(STYLE)>
                topcoat::runtime::script()
                topcoat::dev::script()
            </head>
            <body>
                (escape_manager_script(cx).await?)
                (soft_nav_script(cx).await?)
                (content)
            </body>
        </html>
    }
}
