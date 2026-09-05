//! The document shell every page renders inside, ported from
//! `iz-web/src/app.rs`'s `shell`/`App`/`NotFound`.

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

use crate::i18n::{Key, Lang, t};
use crate::server::current_user;

/// The İz monogram again, as the tab icon: the same drawing as `wordmark`,
/// inlined because it must carry its own colours — a data URI has no page to
/// inherit `currentColor` or the accent token from, so the two themes are
/// spelled out in a media query inside the SVG.
const FAVICON: &str = "data:image/svg+xml,\
    <svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 24 24'>\
    <style>.i{fill:%231a1b1d}.t{fill:%2346557b}.s{stroke:%231a1b1d}\
    @media(prefers-color-scheme:dark){.i{fill:%23e6e4de}.t{fill:%238ba1e8}\
    .s{stroke:%23e6e4de}}</style>\
    <rect class='i' x='3.1' y='9.4' width='4.4' height='11.6' rx='2.2'/>\
    <circle class='t' cx='5.3' cy='4.9' r='2.6'/>\
    <path class='s' d='M11.4 10.6h9.3L11.4 19.8h9.5' fill='none' \
    stroke-width='2.5' stroke-linecap='round' stroke-linejoin='round'/></svg>";

/// The İz monogram — a dotted `i` beside a stroked `z`, the trace the product
/// is named for. It takes `currentColor` for its strokes and the accent for
/// its tittle, so one drawing serves both skins and both themes.
///
/// The monogram and the wordmark are alternates, never a pair: `İz` spells the
/// first two letters of `İz`, so standing them side by side reads as a
/// stutter no amount of space or framing repairs. The mark carries the chrome,
/// where the name is already known and the room is 44px; the word carries the
/// front door, where a stranger arrives and nothing else has said it yet.
pub(crate) async fn mark(cx: &Cx) -> Result {
    view! {
        cx =>
        <a class="wordmark" href="/" aria-label="İz">
            <svg class="wordmark-mark" width="24" height="24" viewBox="0 0 24 24"
                aria-hidden="true">
                <rect x="3.1" y="9.4" width="4.4" height="11.6" rx="2.2"
                    fill="currentColor"></rect>
                <circle class="wordmark-tittle" cx="5.3" cy="4.9" r="2.6"></circle>
                <path d="M11.4 10.6h9.3L11.4 19.8h9.5" fill="none"
                    stroke="currentColor" stroke-width="2.5" stroke-linecap="round"
                    stroke-linejoin="round"></path>
            </svg>
        </a>
    }
}

/// The wordmark, the monogram's other half: the name alone, in the one face
/// both skins agree on. The sign-in and setup pages wear it — see `mark`.
pub(crate) async fn wordmark(cx: &Cx) -> Result {
    view! {
        cx =>
        <a class="wordmark wordmark-lone" href="/">
            <span class="wordmark-text">"İz"</span>
        </a>
    }
}

/// A person as a circle (or, on the Instrument skin, a square): the initials,
/// toned by the id so every account has its own colour, with the im photo
/// over it when im has one. Whether im has one is not known cheaply — the
/// photo is the app's credentialed fetch, not the browser's — so the `<img>`
/// always renders and hides itself on error (see `avatar_script`), leaving
/// the initials beneath as the fallback. Shared by the board, the modal and
/// every topbar user menu.
pub(crate) async fn avatar(cx: &Cx, id: &str, display_name: &str, extra: &str) -> Result {
    let tone = id
        .bytes()
        .fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32))
        % 5;
    let class = format!("avatar avatar-tone-{tone} {extra}");
    let initials = initials_of(display_name);
    let src = format!("/avatar/{id}");
    view! {
        cx =>
        <span class="avatar-stack">
            <span class=(class.clone()) data-name=(display_name.to_string())>(initials)</span>
            <img class=(format!("{class} avatar-photo")) src=(src) alt="" data-name=(display_name.to_string())>
        </span>
    }
}

/// The first letters of the first two words, uppercased — "Ada Lovelace"
/// wears "AL", a bare address wears its first letter.
fn initials_of(name: &str) -> String {
    let mut out = String::new();
    for word in name.split_whitespace().take(2) {
        if let Some(first) = word.chars().next() {
            out.extend(first.to_uppercase());
        }
    }
    if out.is_empty() {
        out.push('?');
    }
    out
}

/// The photo half's client half: a missing photo must fall back to the
/// initials beneath it, and a bare `<img>` to a 404 shows the broken-image
/// box instead. One document-level capture `error` listener hides every
/// `img.avatar-photo` that fails, plus a sweep for the ones already failed
/// before it ran; hiding is registered through `__izOwn` so the live morph
/// — which owns no names, only what the client declares — does not strip it
/// back off on the next refresh.
pub(crate) async fn avatar_script(cx: &Cx) -> Result {
    use topcoat::view::Unescaped;
    const JS: &str = "\
        (function () { \
            if (window.__izAvatar) { return; } \
            window.__izAvatar = true; \
            function hide(img) { \
                if (window.__izOwn) { window.__izOwn(img, [], ['style']); } \
                img.style.display = 'none'; \
            } \
            document.addEventListener('error', function (e) { \
                var img = e.target && e.target.closest ? e.target.closest('img.avatar-photo') : null; \
                if (img) { hide(img); } \
            }, true); \
            document.querySelectorAll('img.avatar-photo').forEach(function (img) { \
                if (img.complete && img.naturalWidth === 0) { hide(img); } \
            }); \
        })();";
    view! { cx => <script>(Unescaped::new_unchecked(JS))</script> }
}

/// The topbar's signed-in identity: the display name, opening on hover or
/// focus onto details (name, address, role) and sign-out. Shared by every
/// signed-in page's topbar.
pub async fn user_menu(cx: &Cx, me: &crate::detail::Me, lang: Lang) -> Result {
    let role_key = match me.role {
        iz_core::Role::Admin => Key::RoleAdminOption,
        iz_core::Role::Member => Key::RoleMemberOption,
        iz_core::Role::Viewer => Key::RoleViewerOption,
    };
    view! {
        cx =>
        <div class="user-menu">
            <button type="button" class="user-menu-trigger">
                (avatar(cx, &me.id, &me.display_name, "").await?)
                <span class="user-menu-trigger-name">(me.display_name.clone())</span>
            </button>
            <div class="user-menu-panel">
                <div class="user-menu-name">(me.display_name.clone())</div>
                <div class="user-menu-email">(me.email.clone())</div>
                <div class="user-menu-role">(t(lang, role_key))</div>
                <a class="user-menu-item" href=(format!("{}/", crate::server::config(cx).oidc.issuer)) data-hard="">(t(lang, Key::Profile))</a>
                <a class="user-menu-item" href="/settings">(t(lang, Key::NavSettings))</a>
                <a class="user-menu-item" href="/auth/logout" data-hard="">(t(lang, Key::SignOut))</a>
            </div>
        </div>
    }
}

/// The signed-in pages, as the topbar nav links between them.
#[derive(Clone, Copy, PartialEq)]
pub enum NavPage {
    Board,
    Rules,
    Logs,
    Tags,
    Settings,
}

impl NavPage {
    const ALL: [Self; 5] = [
        Self::Board,
        Self::Rules,
        Self::Logs,
        Self::Tags,
        Self::Settings,
    ];

    fn href(self) -> &'static str {
        match self {
            Self::Board => "/",
            Self::Rules => "/rules",
            Self::Logs => "/logs",
            Self::Tags => "/tags",
            Self::Settings => "/settings",
        }
    }

    fn label(self) -> Key {
        match self {
            Self::Board => Key::NavBoard,
            Self::Rules => Key::NavMailRules,
            Self::Logs => Key::NavLogs,
            Self::Tags => Key::NavTags,
            Self::Settings => Key::NavSettings,
        }
    }
}

/// The topbar's page nav, shared by every signed-in page: the pages with the
/// current one marked, the admin-only ones (rules, logs, tags) shown only to
/// roles that can administer. Plain `<a>`s — the soft-nav forwarder swaps them
/// like any same-origin link, so `data-hard` stays off.
pub async fn topbar_nav(cx: &Cx, active: NavPage, role: iz_core::Role, lang: Lang) -> Result {
    view! {
        cx =>
        <nav class="topbar-nav-links">
            for page in NavPage::ALL {
                if role.can_administer() || !matches!(page, NavPage::Rules | NavPage::Logs | NavPage::Tags) {
                <a
                    class=(if page == active { "topbar-nav topbar-nav-on" } else { "topbar-nav" })
                    href=(page.href())
                >
                    (t(lang, page.label()))
                </a>
                }
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
/// The replay declares `Accept: text/html`, because a replayed form post is a
/// form post asking for the page back: `carry_refusal_on_redirect`
/// (`server.rs`) copies a refusing 303's answer onto the `Location` only for a
/// caller that wants the page, and a bare `fetch` asks for `*/*` — under which
/// the refusal stayed in a body nobody reads and the click looked like nothing
/// happening. The password pane was where this was first felt: a wrong
/// current password and a right one landed on the same silent page.
///
/// Every fetch that ends in a swap carries the navigation counter it started
/// under (`window.__izNav`), and its answer is thrown away if that counter
/// has moved. Without it the newest paint is not the newest intent: the live
/// refresh fetches the URL it was on, and a click landing during that flight
/// pushed a new page that the refresh's answer then overwrote — the page
/// arriving and reverting to the previous one two hundred milliseconds later.
/// Navigations, form posts and the overlay closes each step the counter, so a
/// second click also wins over a first one still in flight, and a refresh —
/// which is not an intent, only an update of what is already on screen —
/// steps nothing and lands only if nothing else has happened.
///
/// The address bar and the `<html>` attributes are rewritten *before* the
/// new body's scripts run, never after: a swapped-in script that reads
/// `location.href` (the log-fit reload in `logs.rs`) would otherwise read
/// the page it just replaced and hard-navigate back to it.
///
/// Swapped-in `<script>` nodes are inert (parser-inserted only), so
/// `swap()` re-creates each one as a fresh element, which runs it; every
/// emitted script carries a one-shot `window.__iz*` guard, so that
/// re-execution never stacks duplicate document-level listeners. There
/// is no topcoat re-hydrate entry, so per-element behavior is re-run by
/// the `iz:wire` event (`dropdown.rs`, the audio player) over the new
/// nodes.
///
/// Forms that must really navigate (sign-in/out: the session and the whole
/// page identity change) opt out with `data-hard`.
/// Overlay closes never hit the server at all: the board is already
/// rendered under the modal, so `__izCloseModal`/`__izCloseViewer`
/// drop the overlay's DOM and rewrite the URL.
///
/// The click-forwarder at the end is the repo-wide fix for the dead-zone
/// class the member-role select first showed: any click inside a
/// `.field-box`/`.status-form`/`.member-role` that misses the `.dd-trigger`
/// (label, chevron glyph, box padding) is forwarded to the trigger.
///
/// Ordinary same-origin links get the same treatment: a delegated capture
/// click listener — registered after the close/scrim one, and skipped when
/// that one already `preventDefault`ed — fetches the href and swaps it in
/// under a `history.pushState`, so board cards and file chips no longer
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
            if (window.__izSoft) { return; } \
            window.__izSoft = true; \
            window.__izBuild = document.documentElement.getAttribute('data-build') || ''; \
            window.__izOwn = function (node, classes, attrs) { \
                var own = node.__izMine; \
                if (!own) { own = node.__izMine = { c: [], a: [] }; } \
                (classes || []).forEach(function (c) { \
                    if (own.c.indexOf(c) === -1) { own.c.push(c); } \
                    node.classList.add(c); \
                }); \
                (attrs || []).forEach(function (a) { if (own.a.indexOf(a) === -1) { own.a.push(a); } }); \
                return node; \
            }; \
            window.__izAdded = function (node) { node.__izAdded = true; return node; }; \
            function wire() { \
                document.querySelectorAll('.comment-list').forEach(function (list) { list.scrollTop = list.scrollHeight; }); \
                document.dispatchEvent(new Event('iz:wire')); \
            } \
            function keyOf(el) { \
                var form = el.form ? (el.form.getAttribute('action') || '') : ''; \
                return form + '|' + (el.name || '') + '|' + (el.type || '') + '|' + (el.tagName || ''); \
            } \
            function editable(el) { \
                return el.type !== 'hidden' && el.type !== 'password' && el.type !== 'file'; \
            } \
            function captureFields() { \
                var active = document.activeElement, out = []; \
                document.querySelectorAll('input, textarea, select').forEach(function (el) { \
                    if (!editable(el)) { return; } \
                    var moved; \
                    if (el.type === 'checkbox' || el.type === 'radio') { moved = el.checked !== el.defaultChecked; } \
                    else if (el.tagName === 'SELECT') { \
                        var def = -1; \
                        for (var j = 0; j < el.options.length; j++) { if (el.options[j].defaultSelected) { def = j; break; } } \
                        if (def < 0) { def = 0; } \
                        moved = el.selectedIndex !== def; \
                    } else { moved = el.value !== el.defaultValue; } \
                    var focused = el === active; \
                    if (!moved && !focused) { return; } \
                    var start = -1, end = -1; \
                    try { if (el.selectionStart != null) { start = el.selectionStart; end = el.selectionEnd; } } catch (err) { } \
                    out.push({ k: keyOf(el), v: el.value, c: el.checked, i: el.selectedIndex, f: focused, s: start, e: end }); \
                }); \
                return out; \
            } \
            function restoreFields(saved) { \
                if (!saved || !saved.length) { return; } \
                var fields = document.querySelectorAll('input, textarea, select'); \
                saved.forEach(function (was) { \
                    var el = null; \
                    for (var i = 0; i < fields.length; i++) { \
                        if (keyOf(fields[i]) === was.k) { el = fields[i]; break; } \
                    } \
                    if (!el) { return; } \
                    if (el.type === 'checkbox' || el.type === 'radio') { el.checked = was.c; } \
                    else if (el.tagName === 'SELECT') { el.selectedIndex = was.i; } \
                    else { el.value = was.v; } \
                    if (was.f) { \
                        try { \
                            el.focus(); \
                            if (was.s >= 0 && el.setSelectionRange) { el.setSelectionRange(was.s, was.e); } \
                        } catch (err) { } \
                    } \
                }); \
            } \
            function clientMade(node) { return node.__izAdded === true; } \
            function pairable(a, b) { \
                if (a.nodeType !== b.nodeType) { return false; } \
                if (a.nodeType !== 1) { return true; } \
                if (a.nodeName !== b.nodeName) { return false; } \
                if (a.id || b.id) { return a.id === b.id; } \
                return true; \
            } \
            function syncClass(from, to, own) { \
                var want = []; \
                to.classList.forEach(function (c) { want.push(c); }); \
                if (own) { \
                    own.c.forEach(function (c) { \
                        if (from.classList.contains(c) && want.indexOf(c) === -1) { want.push(c); } \
                    }); \
                } \
                if (want.length) { from.setAttribute('class', want.join(' ')); } \
                else if (from.hasAttribute('class')) { from.removeAttribute('class'); } \
            } \
            function syncAttrs(from, to) { \
                var own = from.__izMine, i, at; \
                for (i = to.attributes.length - 1; i >= 0; i--) { \
                    at = to.attributes[i]; \
                    if (at.name === 'class') { continue; } \
                    if (own && own.a.indexOf(at.name) !== -1) { continue; } \
                    if (from.getAttribute(at.name) !== at.value) { from.setAttribute(at.name, at.value); } \
                } \
                for (i = from.attributes.length - 1; i >= 0; i--) { \
                    at = from.attributes[i]; \
                    if (at.name === 'class') { continue; } \
                    if (own && own.a.indexOf(at.name) !== -1) { continue; } \
                    if (!to.hasAttribute(at.name)) { from.removeAttribute(at.name); } \
                } \
                syncClass(from, to, own); \
            } \
            function morph(from, to) { \
                if (from.nodeType !== 1) { \
                    if (from.nodeValue !== to.nodeValue) { from.nodeValue = to.nodeValue; } \
                    return; \
                } \
                syncAttrs(from, to); \
                if (from.nodeName === 'SCRIPT') { \
                    if (from.textContent !== to.textContent) { \
                        var live = document.createElement('script'); \
                        if (to.hasAttribute('src')) { live.setAttribute('src', to.getAttribute('src')); } \
                        live.textContent = to.textContent; \
                        from.replaceWith(live); \
                    } \
                    return; \
                } \
                morphChildren(from, to); \
            } \
            function morphChildren(from, to) { \
                var mine = [], theirs = [], n; \
                for (n = from.firstChild; n; n = n.nextSibling) { if (!clientMade(n)) { mine.push(n); } } \
                for (n = to.firstChild; n; n = n.nextSibling) { theirs.push(n); } \
                var i; \
                for (i = 0; i < theirs.length; i++) { \
                    if (i < mine.length) { \
                        if (pairable(mine[i], theirs[i])) { morph(mine[i], theirs[i]); } \
                        else { mine[i].replaceWith(document.importNode(theirs[i], true)); } \
                    } else { \
                        from.appendChild(document.importNode(theirs[i], true)); \
                    } \
                } \
                for (i = theirs.length; i < mine.length; i++) { mine[i].remove(); } \
            } \
            window.__izNav = 0; \
            function navStep() { return ++window.__izNav; } \
            function stillCurrent(n) { return window.__izNav === n; } \
            function swap(html, url, fresh, push, morphing) { \
                var doc = new DOMParser().parseFromString(html, 'text/html'); \
                var build = doc.documentElement.getAttribute('data-build') || ''; \
                if (build && window.__izBuild && build !== window.__izBuild) { \
                    var dirty = captureFields().length > 0; \
                    var reloaded = null; \
                    try { reloaded = sessionStorage.getItem('izSkewFor'); } catch (err) { } \
                    if (!dirty && reloaded !== build) { \
                        try { sessionStorage.setItem('izSkewFor', build); } catch (err) { } \
                        location.reload(); \
                        return; \
                    } \
                    if (!dirty) { \
                        window.__izBuild = build; \
                        document.documentElement.setAttribute('data-build', build); \
                    } \
                } \
                var keep = window.__izKeep ? captureFields() : null; \
                window.__izKeep = false; \
                var x = window.scrollX, y = window.scrollY; \
                var root = doc.documentElement; \
                if (root.getAttribute('lang')) { document.documentElement.setAttribute('lang', root.getAttribute('lang')); } \
                if (root.hasAttribute('data-theme')) { document.documentElement.setAttribute('data-theme', root.getAttribute('data-theme')); } \
                else { document.documentElement.removeAttribute('data-theme'); } \
                if (root.hasAttribute('data-ui')) { document.documentElement.setAttribute('data-ui', root.getAttribute('data-ui')); } \
                else { document.documentElement.removeAttribute('data-ui'); } \
                if (url) { history[push ? 'pushState' : 'replaceState'](null, '', url); } \
                if (morphing) { \
                    morph(document.body, doc.body); \
                } else { \
                    document.body.replaceChildren(); \
                    while (doc.body.firstChild) { document.body.appendChild(doc.body.firstChild); } \
                    document.body.querySelectorAll('script').forEach(function (old) { \
                        var live = document.createElement('script'); \
                        if (old.hasAttribute('src')) { live.setAttribute('src', old.getAttribute('src')); } \
                        live.textContent = old.textContent; \
                        old.replaceWith(live); \
                    }); \
                    window.scrollTo(fresh ? 0 : x, fresh ? 0 : y); \
                } \
                restoreFields(keep); \
                wire(); \
            } \
            function sweepPanels() { \
                document.querySelectorAll('.dd-panel').forEach(function (panel) { \
                    if (panel.__ddTrigger && !document.contains(panel.__ddTrigger)) { panel.remove(); } \
                }); \
            } \
            window.__izGo = function (url) { \
                var n = navStep(); \
                fetch(url).then( \
                    function (r) { return r.text().then(function (t) { if (stillCurrent(n)) { swap(t, r.url); } }); }, \
                    function () { window.location.href = url; } \
                ); \
            }; \
            window.__izRefresh = function () { \
                var n = window.__izNav; \
                fetch(window.location.href).then( \
                    function (r) { return r.text().then(function (t) { if (stillCurrent(n)) { swap(t, r.url, false, false, true); } }); }, \
                    function () {} \
                ); \
            }; \
            window.__izQuery = function (url) { \
                var n = navStep(); \
                fetch(url).then( \
                    function (r) { return r.text().then(function (t) { if (stillCurrent(n)) { swap(t, r.url, false, false, true); } }); }, \
                    function () {} \
                ); \
            }; \
            window.__izPost = function (action, fields) { \
                var n = navStep(); \
                fetch(action, { method: 'POST', headers: { accept: 'text/html' }, body: new URLSearchParams(fields) }).then( \
                    function (r) { return r.text().then(function (t) { if (stillCurrent(n)) { swap(t, r.url); } }); }, \
                    function () {} \
                ); \
            }; \
            window.__izCloseViewer = function () { \
                var scrim = document.querySelector('.viewer-scrim'); \
                if (!scrim) { return false; } \
                var back = scrim.querySelector('.viewer-close').getAttribute('href'); \
                navStep(); \
                scrim.remove(); \
                sweepPanels(); \
                history.replaceState(null, '', back); \
                var modal = document.querySelector('.modal'); \
                if (modal) { modal.focus(); } \
                return true; \
            }; \
            window.__izCloseModal = function () { \
                var scrims = document.querySelectorAll('.modal-scrim'); \
                if (!scrims.length) { return false; } \
                navStep(); \
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
                    window.__izGo((form.getAttribute('action') || window.location.pathname) + (q ? '?' + q : '')); \
                    return; \
                } \
                e.preventDefault(); \
                var data = e.submitter ? new FormData(form, e.submitter) : new FormData(form); \
                var multipart = (form.getAttribute('enctype') || '').indexOf('multipart') !== -1; \
                if (multipart) { \
                    if (form.__izUploading) { return; } \
                    form.__izUploading = true; \
                    var n = navStep(); \
                    var box = form.querySelector('.file-upload-box'); \
                    var input = form.querySelector('.file-upload-input'); \
                    var bar = null, drop = null; \
                    if (box) { \
                        bar = document.createElement('div'); \
                        bar.className = 'upload-progress'; \
                        var fill = document.createElement('div'); \
                        fill.className = 'upload-progress-fill'; \
                        bar.appendChild(fill); \
                        box.appendChild(window.__izAdded(bar)); \
                        drop = document.createElement('button'); \
                        drop.type = 'button'; \
                        drop.className = 'file-chip-drop upload-progress-drop'; \
                        var cancelLabel = form.getAttribute('data-cancel-label'); \
                        if (cancelLabel) { drop.setAttribute('aria-label', cancelLabel); } \
                        drop.innerHTML = '<svg class=\"glyph\" width=\"13\" height=\"13\" viewBox=\"0 0 16 16\" fill=\"none\" stroke=\"currentColor\" stroke-width=\"1.5\" stroke-linecap=\"round\" aria-hidden=\"true\"><path d=\"M4 4l8 8M12 4l-8 8\"></path></svg>'; \
                        drop.addEventListener('click', function (e) { e.preventDefault(); e.stopPropagation(); x.abort(); }); \
                        box.appendChild(window.__izAdded(drop)); \
                    } \
                    if (input) { input.disabled = true; window.__izOwn(input, [], ['disabled']); } \
                    var settle = function () { \
                        form.__izUploading = false; \
                        if (input) { input.disabled = false; } \
                        if (bar && bar.parentNode) { bar.parentNode.removeChild(bar); } \
                        if (drop && drop.parentNode) { drop.parentNode.removeChild(drop); } \
                    }; \
                    var x = new XMLHttpRequest(); \
                    x.open('POST', form.getAttribute('action') || window.location.href); \
                    x.setRequestHeader('accept', 'text/html'); \
                    x.upload.onprogress = function (ev) { \
                        if (!fill) { return; } \
                        if (!bar.isConnected) { box.appendChild(bar); if (drop) { box.appendChild(drop); } } \
                        if (ev.lengthComputable && ev.total > 0) { fill.style.width = Math.min(100, Math.round((ev.loaded / ev.total) * 100)) + '%'; } \
                    }; \
                    x.onload = function () { \
                        settle(); \
                        if (!stillCurrent(n)) { return; } \
                        swap(x.responseText, x.responseURL || form.getAttribute('action') || window.location.href); \
                    }; \
                    x.onerror = function () { settle(); form.submit(); }; \
                    x.onabort = function () { \
                        if (input) { input.value = ''; } \
                        var name = box ? box.querySelector('.file-upload-name') : null; \
                        var empty = form.getAttribute('data-empty-label'); \
                        if (name && empty) { name.textContent = empty; } \
                        settle(); \
                    }; \
                    x.send(data); \
                    return; \
                } \
                var n = navStep(); \
                fetch(form.getAttribute('action'), { method: 'POST', headers: { accept: 'text/html' }, body: new URLSearchParams(data) }).then( \
                    function (r) { return r.text().then(function (t) { if (stillCurrent(n)) { swap(t, r.url); } }); }, \
                    function () { form.submit(); } \
                ); \
            }, true); \
            document.addEventListener('change', function (e) { \
                var control = e.target; \
                if (control.classList && control.classList.contains('file-upload-input')) { \
                    var label = control.closest('label'); \
                    var name = label ? label.querySelector('.file-upload-name') : null; \
                    if (name && control.files && control.files[0]) { \
                        name.textContent = control.files[0].name + (control.files.length > 1 ? ' +' + (control.files.length - 1) : ''); \
                    } \
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
                        window.__izCloseViewer(); \
                        return; \
                    } \
                    if (link.getAttribute('href') === '/' && link.closest('.modal-scrim')) { \
                        e.preventDefault(); \
                        window.__izCloseModal(); \
                        return; \
                    } \
                } \
                if (e.target.classList && e.target.classList.contains('modal-scrim')) { \
                    if (e.target.classList.contains('viewer-scrim')) { window.__izCloseViewer(); } \
                    else { window.__izCloseModal(); } \
                } \
            }, true); \
            window.addEventListener('popstate', function () { window.__izGo(window.location.href); }); \
            document.addEventListener('click', function (e) { \
                if (e.defaultPrevented || e.button !== 0 || e.ctrlKey || e.metaKey || e.shiftKey || e.altKey) { return; } \
                var link = e.target.closest ? e.target.closest('a') : null; \
                if (!link || link.hasAttribute('download') || link.hasAttribute('target') || link.hasAttribute('data-hard')) { return; } \
                var href = link.getAttribute('href'); \
                if (!href || href.charAt(0) !== '/' || href.indexOf('/files/') === 0) { return; } \
                e.preventDefault(); \
                var n = navStep(); \
                fetch(href).then( \
                    function (r) { return r.text().then(function (t) { if (stillCurrent(n)) { swap(t, r.url, true, true); } }); }, \
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
/// `window.__izEsc.register(priority, fn)` and on `Escape` the manager
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
/// The live channel's browser half: one `EventSource` per tab, and the slow
/// tick that keeps clock-driven text honest.
///
/// Everything here is one script because both halves end in the same act —
/// re-fetch the current URL through `__izGo` and let `swap()` do what it
/// already does after every form post. There is deliberately no second render
/// path: a partial-update mechanism would be a second way for the page to be
/// wrong, and the full swap is the one the app exercises constantly.
///
/// Three things it must not get wrong:
///
/// The `__izLive` guard is not decoration. `swap()` re-executes every
/// swapped-in script, so without it each soft navigation would open another
/// `EventSource` and the tab would end up holding one connection per page it
/// had ever visited.
///
/// A refresh must not disturb what somebody is in the middle of. The first
/// attempt held the refresh back while a form was dirty, which was wrong twice
/// over: one unsubmitted form froze every other surface on the screen, and a
/// dropdown could never be used at all while the workspace was busy, because
/// each update slammed it shut.
///
/// The cause of both was the refresh throwing the whole page away. So the live
/// path does not: it MORPHS. `swap`'s morphing mode walks the freshly fetched
/// document against the live one and touches only what actually differs —
/// changed text, changed attributes, added and removed nodes. Everything else
/// is the same DOM node it was a moment ago, so an open dropdown stays open, a
/// caret stays where it was, a half-typed comment keeps its text, and a scrolled
/// panel keeps its position. Not because any of them is special-cased, but
/// because nothing touched them.
///
/// A `<script>` whose text is unchanged is left alone rather than re-created,
/// so the live path stops re-running every script on the page; the `__iz*`
/// guards remain the backstop for the full-swap path, which still re-runs them.
///
/// ## The client declares what is its own
///
/// A morph rebuilds the page from HTML that knows nothing about the client.
/// Two things it would therefore destroy: nodes the client created, which
/// look like strays, and the marks an enhancement leaves on a node the server
/// *did* render — `dd-native` hiding a replaced select, `data-wired` on a
/// wired player, the position an open context menu was placed at.
///
/// The first version of this knew their names: `clientMade` tested for
/// `.dd-panel`/`.dd-trigger`, and the attribute sweep skipped anything
/// starting with `data-dd`. That is a list, and a list is wrong the first
/// time somebody adds an enhancement without reading it. It was already
/// wrong: `data-dd-done` was on the list and `class` was not, so a save
/// unhid every native select underneath its own trigger while `enhanceAll`
/// skipped them as already done — one dropdown drawn twice, per field.
///
/// So the morph knows no names at all. A node says what belongs to the
/// client, at the moment the client takes it:
///
/// - `__izOwn(node, classes, attrs)` — the classes and attributes on this
///   server-rendered node are the client's. It adds the classes itself, so
///   there is no way to apply one without registering it.
/// - `__izAdded(node)` — this whole node is the client's; it is not a
///   stray and must not be deleted.
///
/// Both record on a DOM *property*, which no morph can strip, and both are
/// defined here — before any script that could enhance anything runs.
/// `syncAttrs` then merges `class` (the server's list plus whatever owned
/// classes are still on the node) instead of overwriting it, and skips owned
/// attributes both when it copies the server's over and when it sweeps the
/// ones the server no longer has — an owned attribute is the client's fact,
/// stale server bytes say nothing against it. Nothing in this file mentions
/// a dropdown.
///
/// The other half of the contract is `iz:wire`: an enhancement re-derives
/// whatever it draws from server data rather than assuming it is still true.
/// Ownership keeps the morph from breaking the enhancement; the wire pass is
/// how the enhancement follows the server when the data underneath it moves.
///
/// Morphing is used only for the live refresh. Navigations and form posts keep
/// the full replace: they are a different page or a submitted form, where
/// carrying the old DOM's state over is wrong rather than kind.
///
/// The tick exists because some text goes stale with no write behind it: the
/// queue's next-try time and a card's overdue mark change because the clock
/// moved, and no announcement will ever fire for that. Rather than re-format
/// those in JavaScript — which would mean a second implementation of the
/// locale- and timezone-aware formatting the server already does, free to
/// disagree with it — the tick re-fetches, and the server stays the only thing
/// that decides what a moment reads as. It runs only on pages carrying a
/// `data-tick` element, so a page with no clock-driven text is silent.
pub async fn live_script(cx: &Cx) -> Result {
    const JS: &str = "\
        (function () { \
            if (window.__izLive) { return; } \
            window.__izLive = true; \
            var timer = null; \
            function refresh() { \
                window.__izKeep = true; \
                if (window.__izRefresh) { window.__izRefresh(); } \
            } \
            function schedule() { \
                if (timer) { clearTimeout(timer); } \
                timer = setTimeout(function () { timer = null; refresh(); }, 200); \
            } \
            function wanted(topic) { \
                if (topic === 'resync') { return true; } \
                var path = window.location.pathname; \
                if (path === '/logs') { return topic !== 'board' && topic !== 'task'; } \
                if (path === '/rules') { return topic === 'rules' || topic === 'members'; } \
                if (path === '/tags') { return topic === 'tags' || topic === 'members'; } \
                if (path === '/settings') { return topic === 'settings' || topic === 'members'; } \
                if (path === '/people/') { return topic === 'members'; } \
            } \
            try { \
                var src = new EventSource('/api/live'); \
                src.onmessage = function (e) { \
                    var frame; \
                    try { frame = JSON.parse(e.data); } catch (err) { return; } \
                    if (frame && wanted(frame.topic)) { schedule(); } \
                }; \
            } catch (err) { } \
            setInterval(function () { \
                if (document.querySelector('[data-tick]')) { refresh(); } \
            }, 60000); \
        })(); \
    ";
    view! { cx => <script>(Unescaped::new_unchecked(JS))</script> }
}

pub async fn escape_manager_script(cx: &Cx) -> Result {
    use topcoat::view::Unescaped;
    const JS: &str = "\
        (function () { \
        if (window.__izEsc) { return; } \
        window.__izEsc = { \
            resolvers: [], \
            register: function (priority, fn) { \
                window.__izEsc.resolvers.push({ priority: priority, fn: fn }); \
                window.__izEsc.resolvers.sort(function (a, b) { return b.priority - a.priority; }); \
            } \
        }; \
        document.addEventListener('keydown', function (e) { \
            if (e.key !== 'Escape') { return; } \
            for (var i = 0; i < window.__izEsc.resolvers.length; i++) { \
                if (window.__izEsc.resolvers[i].fn(e)) { \
                    e.preventDefault(); \
                    e.stopImmediatePropagation(); \
                    return; \
                } \
            } \
        }, true); \
        })();";
    view! { cx => <script>(Unescaped::new_unchecked(JS))</script> }
}
/// Registers the topbar/rules `Escape` resolvers on `window.__izEsc`
/// (priority 40 — the table is on `escape_manager_script`): the topbar
/// `.user-menu` panel — hover-open included, pinned shut by a
/// `user-menu-esc` class that a `mouseenter` inside the menu clears — then
/// a `.rule-new` composer left open on the rules page, then a rules edit
/// row (navigating back to `/rules`). Never touches
/// `details.confirm-details`; that flow stays `detail.rs`'s.
pub async fn escape_script(cx: &Cx) -> Result {
    const JS: &str = "\
        (function () { \
        if (window.__izEscTop) { return; } \
        window.__izEscTop = true; \
        window.__izEsc.register(40, function () { \
            var menu = document.querySelector('.user-menu'); \
            if (menu && (menu.matches(':hover') || menu.contains(document.activeElement))) { \
                window.__izOwn(menu, ['user-menu-esc'], []); \
                var focused = document.activeElement; \
                if (focused && focused.closest('.user-menu')) { focused.blur(); } \
                return true; \
            } \
            var composer = document.querySelector('details.rule-new[open]'); \
            if (composer) { composer.removeAttribute('open'); return true; } \
            if (document.querySelector('.rule-new-body[action=\"/api/update_rule\"]')) { \
                if (window.__izGo) { window.__izGo('/rules'); } else { window.location.href = '/rules'; } \
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

    // The build stamp rides the html element on every render — page load,
    // soft navigation and live refresh all answer through this layout, and
    // the swap path reads it off the fetched document before it touches the
    // page. A tab left open across a deploy keeps the old stylesheet link
    // (the morph swaps body children, never the head), so a mismatch is
    // the client's cue to hard-reload rather than wear the old css.
    let build = STYLE.id().as_u64().to_string();
    view! {
        <!DOCTYPE html>
        <html lang=(lang.code()) data-theme=(dark.then_some("dark")) data-ui=(ui) data-build=(build)>
            <head>
                <meta charset="utf-8">
                <meta name="viewport" content="width=device-width, initial-scale=1">
                <link rel="preconnect" href="https://fonts.googleapis.com">
                <link rel="preconnect" href="https://fonts.gstatic.com" crossorigin="">
                <link
                    rel="stylesheet"
                    href="https://fonts.googleapis.com/css2?family=IBM+Plex+Mono:wght@400;500&family=Newsreader:ital,wght@0,400;0,600;1,400;1,600&display=swap"
                >
                <title>"İz"</title>
                <link rel="icon" href=(FAVICON)>
                <link rel="stylesheet" href=(STYLE)>
                topcoat::runtime::script()
                topcoat::dev::script()
            </head>
            <body>
                (escape_manager_script(cx).await?)
                (soft_nav_script(cx).await?)
                // Only for a signed-in page: `/api/live` answers 401 to
                // everybody else, and an auth screen that opened a stream
                // would just reconnect against that refusal forever.
                if asking.is_some() { (live_script(cx).await?) }
                (content)
            </body>
        </html>
    }
}
