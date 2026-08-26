//! Server-side plumbing for the auth surface: the session cookie, the person
//! behind the current request, and the role guards.
//!
//! Every guard here is the real one. The UI hides what a role may not do, but
//! the answer that matters is the one given in this module, on the server.

use axum::http::HeaderValue;
use axum::http::header::{COOKIE, SET_COOKIE};
use axum::http::request::Parts;
use izlek_core::accounts::{AccountError, Accounts};
use izlek_core::board::Transition;
use izlek_core::mail::Engine;
use izlek_core::store::{Freeing, User};
use leptos::prelude::*;
use leptos_axum::ResponseOptions;

/// The session cookie's name. One cookie, one browser, one session row.
pub const SESSION_COOKIE: &str = "izlek_session";

/// The workspace's account service, put into context by the router.
pub fn accounts() -> Accounts {
    expect_context::<Accounts>()
}

/// The mail engine, or the fact that there is nobody to hand a crossing to.
///
/// The running server always has an engine: the sender is workspace settings
/// now, so it can appear at any moment and the engine reads it per send. The
/// `None` is for tests, which drive the router without one and assert on the
/// ledger rather than on a mail server.
#[derive(Clone)]
pub struct Mail(Option<std::sync::Arc<Engine>>);

impl Mail {
    /// No engine at all. Crossings are recorded and nothing is handed on.
    pub fn silent() -> Self {
        Self(None)
    }

    pub fn sending(engine: std::sync::Arc<Engine>) -> Self {
        Self(Some(engine))
    }

    /// Hands a committed crossing to the engine, off the request.
    ///
    /// The move is already written by the time this is called and the response
    /// does not wait for it: a card that took thirty seconds to drop because
    /// somebody's SMTP host was slow would be a board broken by its own mail
    /// feature. What the send is owed is in the ledger, so a process that dies
    /// mid-send loses nothing — the sweep picks it up.
    pub fn after(&self, transition: Transition) {
        let Some(engine) = self.0.clone() else {
            return;
        };
        tokio::spawn(async move {
            let report = engine.on_transition(&transition).await;
            Self::log(report);
        });
    }

    /// Kicks the engine to send an invite mail now rather than on the next
    /// sweep, off the request the same way `after` is.
    ///
    /// The invite is already on the ledger by the time this is called — this
    /// only makes the wait before it leaves as short as the request itself,
    /// so the admin does not sit wondering whether the mail is coming.
    pub fn after_invite(&self) {
        let Some(engine) = self.0.clone() else {
            return;
        };
        tokio::spawn(async move {
            let report = engine
                .deliver_owed(time::OffsetDateTime::now_utc(), 8)
                .await;
            Self::log(report);
        });
    }

    /// Hands a committed delete to the engine, off the request, the same way.
    ///
    /// A blocker being deleted frees the tasks that were waiting on it just as
    /// finishing it would, so the unblocked rule fires on both. The freeing is
    /// already written; this only reads it.
    pub fn after_freeing(&self, freeing: Freeing, freed: Vec<String>) {
        let Some(engine) = self.0.clone() else {
            return;
        };
        tokio::spawn(async move {
            let report = engine.on_freeing(&freeing, &freed).await;
            Self::log(report);
        });
    }

    /// Sends one test mail and waits for the answer, because the answer is the
    /// whole point of pressing the button. `None` means this process has no
    /// engine at all, which happens only in tests.
    pub async fn test(&self, to: &str) -> Option<Result<time::Duration, izlek_core::MailError>> {
        let engine = self.0.clone()?;
        Some(engine.send_test(to).await)
    }

    fn log(report: izlek_core::store::Result<izlek_core::mail::Report>) {
        match report {
            Ok(report) if report.sent + report.failed + report.abandoned > 0 => {
                println!(
                    "izlek mail  {} sent, {} to retry, {} given up on",
                    report.sent, report.failed, report.abandoned
                );
            }
            Ok(_) => {}
            Err(problem) => eprintln!("izlek mail  the ledger could not be read: {problem}"),
        }
    }
}

/// The engine for this request, or a silent one when the router was built
/// without an engine at all.
pub fn mail() -> Mail {
    use_context::<Mail>().unwrap_or_else(Mail::silent)
}

/// The cookie value this request presented, if it presented one.
pub fn presented_session() -> Option<String> {
    session_in(&use_context::<Parts>()?.headers)
}

/// The same scan as `presented_session`, off a plain `HeaderMap` rather than
/// the leptos request context — for handlers axum calls directly, which have
/// no such context to read.
pub fn session_in(headers: &axum::http::HeaderMap) -> Option<String> {
    for header in headers.get_all(COOKIE) {
        let Ok(raw) = header.to_str() else { continue };
        for pair in raw.split(';') {
            let pair = pair.trim();
            if let Some(value) = pair.strip_prefix(SESSION_COOKIE)
                && let Some(value) = value.strip_prefix('=')
            {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// A stable-enough label for the client, for rate limiting. A proxy header is
/// only trusted because Izlek is meant to sit behind one; the address bucket is
/// the limit that actually protects the Argon2 work either way.
pub fn client_label() -> String {
    let Some(parts) = use_context::<Parts>() else {
        return "unknown".to_string();
    };
    if let Some(forwarded) = parts.headers.get("x-forwarded-for")
        && let Ok(raw) = forwarded.to_str()
        && let Some(first) = raw.split(',').next()
        && !first.trim().is_empty()
    {
        return first.trim().to_string();
    }
    parts
        .extensions
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|info| info.0.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Writes the session cookie. `HttpOnly` so script cannot read it, `Secure` so
/// it never crosses plain HTTP, `SameSite=Lax` so another site's form cannot
/// post with it.
pub fn set_session_cookie(token: &str, lifetime: time::Duration) {
    let value = format!(
        "{SESSION_COOKIE}={token}; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age={}",
        lifetime.whole_seconds().max(0)
    );
    write_cookie(&value);
}

/// Removes the session cookie from this browser. The server-side revocation is
/// what actually ends the session; this only tidies the client.
pub fn clear_session_cookie() {
    write_cookie(&format!(
        "{SESSION_COOKIE}=; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=0"
    ));
}

fn write_cookie(value: &str) {
    let Some(response) = use_context::<ResponseOptions>() else {
        return;
    };
    match HeaderValue::from_str(value) {
        Ok(header) => response.append_header(SET_COOKIE, header),
        Err(_) => {
            // A token is hex, and the rest is a literal, so this cannot happen
            // from user input; refusing to send a malformed header is still the
            // right failure.
            eprintln!("refusing to set a malformed session cookie");
        }
    }
}

/// The person behind this request, or nobody.
pub async fn current_user() -> Option<User> {
    let presented = presented_session()?;
    accounts().authenticate(&presented).await.ok().flatten()
}

/// The person behind this request, or a refusal the caller can return as-is.
pub async fn require_user() -> Result<User, Refusal> {
    current_user().await.ok_or(Refusal::SignInFirst)
}

/// The admin behind this request. A member or a viewer is refused *here*, not
/// merely hidden from in the UI.
pub async fn require_admin() -> Result<User, Refusal> {
    let user = require_user().await?;
    if user.role.can_administer() {
        Ok(user)
    } else {
        Err(Refusal::Forbidden)
    }
}

/// The person behind this request if they may change the board.
pub async fn require_writer() -> Result<User, Refusal> {
    let user = require_user().await?;
    if user.role.can_write_tasks() {
        Ok(user)
    } else {
        Err(Refusal::Forbidden)
    }
}

use crate::auth::Refusal;

impl From<AccountError> for Refusal {
    fn from(error: AccountError) -> Self {
        match error {
            AccountError::Rejected => Refusal::Rejected,
            AccountError::RateLimited => Refusal::RateLimited,
            AccountError::Password(problem) => Refusal::Password(problem.to_string()),
            AccountError::Forbidden => Refusal::Forbidden,
            AccountError::AlreadyClaimed => Refusal::AlreadyClaimed,
            AccountError::AddressTaken => Refusal::AddressTaken,
            AccountError::Store(error) => {
                eprintln!("store error: {error}");
                Refusal::Unavailable
            }
            AccountError::Auth(error) => {
                eprintln!("auth error: {error}");
                Refusal::Unavailable
            }
        }
    }
}

/// The whole application as an axum `Router`: the server functions, the
/// rendered routes and the static files.
///
/// It lives here rather than in `main` so a test can drive the real handlers —
/// the guards above are only worth anything if something calls them the way a
/// browser does.
pub fn router(
    accounts: Accounts,
    mail: Mail,
    leptos_options: LeptosOptions,
) -> axum::Router {
    use axum::Router;
    use leptos_axum::{LeptosRoutes, generate_route_list};

    let routes = generate_route_list(crate::app::App);
    Router::new()
        .route("/healthz", axum::routing::get(|| async { "ok" }))
        .route(
            "/files",
            axum::routing::post(crate::files::upload).layer(
                axum::extract::DefaultBodyLimit::max(
                    crate::settings::WIDEST_ATTACHMENT_MB as usize * 1024 * 1024,
                ),
            ),
        )
        .route("/files/{id}", axum::routing::get(crate::files::download))
        .leptos_routes_with_context(
            &leptos_options,
            routes,
            {
                // Provided here *and* to the server-function handler, which
                // `leptos_routes_with_context` registers with the same closure.
                let accounts = accounts.clone();
                let mail = mail.clone();
                move || {
                    provide_context(accounts.clone());
                    provide_context(mail.clone());
                }
            },
            {
                let leptos_options = leptos_options.clone();
                move || crate::app::shell(leptos_options.clone())
            },
        )
        .fallback(leptos_axum::file_and_error_handler(crate::app::shell))
        .layer(axum::middleware::from_fn(carry_refusal_on_redirect))
        .layer(axum::Extension(accounts))
        .with_state(leptos_options)
}

/// Puts a refusal on the redirect a browser without script follows.
///
/// A hydrated page reads the call's return value straight off the action. A
/// browser without script has no such thing: it posts the form, the server
/// function handler answers `302` back to the page it came from, and the value
/// — the whole refusal — sits in a body nobody will ever look at. The click
/// then looks like nothing happening, which is the worst answer Izlek can give.
///
/// So the refusal is copied onto the `Location`, as `?refusal=<code>&on=<call>`,
/// and the page renders it from the query. This is one place rather than
/// thirty-eight because the shape is the same for every refusing call, present
/// and future: nothing here knows what any of them do.
///
/// Requests carrying script are untouched — they are answered with the value
/// itself and never see a redirect.
async fn carry_refusal_on_redirect(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use crate::auth::{Refusal, call_id};
    use axum::http::StatusCode;
    use axum::http::header::{ACCEPT, LOCATION, REFERER};

    // A form post from a browser asks for a page back. A server-function call
    // from the hydrated bundle does not.
    let wants_page = request
        .headers()
        .get(ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("text/html"));
    let called = call_id(request.uri().path());
    let has_referer = request.headers().contains_key(REFERER);
    let response = next.run(request).await;
    if !wants_page || !has_referer || response.status() != StatusCode::FOUND {
        return response;
    }

    let (mut parts, body) = response.into_parts();
    // The body of one of these redirects is a serialised `Option<Refusal>` and
    // nothing else; the cap is there so a response that is something else
    // entirely cannot be read into memory whole.
    let Ok(bytes) = axum::body::to_bytes(body, 64 * 1024).await else {
        return axum::response::Response::from_parts(parts, axum::body::Body::empty());
    };
    if let Ok(Some(refusal)) = serde_json::from_slice::<Option<Refusal>>(&bytes)
        && let Some(location) = parts.headers.get(LOCATION).and_then(|v| v.to_str().ok())
        && let Some(carried) = carrying(location, refusal.code(), &called)
        && let Ok(value) = HeaderValue::from_str(&carried)
    {
        parts.headers.insert(LOCATION, value);
    }
    axum::response::Response::from_parts(parts, axum::body::Body::from(bytes))
}

/// `location` with the refusal in its query.
///
/// The redirect goes back to the page the form was posted from, and that page
/// may already carry a query — `?task=DZ-01` is how a browser without script
/// opens the modal at all — so the two pairs are merged in, and the pair from
/// any earlier refusal is dropped rather than stacked on top of.
fn carrying(location: &str, code: &str, called: &str) -> Option<String> {
    if called.is_empty() {
        return None;
    }
    // The Location we are rewriting came from the form post's Referer, and on a
    // cross-origin post the Referer is whatever the other site is. Sending the
    // browser back there would make Izlek an open redirect, so the address is
    // rebuilt from its path and query alone and anything that is not a plain
    // absolute path is answered with the board.
    let here = same_origin(location);
    let (path, query) = match here.split_once('?') {
        Some((path, query)) => (path, query),
        None => (here, ""),
    };
    let mut pairs: Vec<String> = query
        .split('&')
        .filter(|pair| {
            !pair.is_empty() && !pair.starts_with("refusal=") && !pair.starts_with("on=")
        })
        .map(str::to_string)
        .collect();
    pairs.push(format!("refusal={code}&on={called}"));
    Some(format!("{path}?{}", pairs.join("&")))
}

/// The path and query of `location`, with scheme and authority dropped. A
/// protocol-relative address (`//elsewhere.example/`) is another host wearing a
/// path's clothes, and a browser reads a backslash there as a slash, so both
/// are answered with the board rather than trusted.
fn same_origin(location: &str) -> &str {
    let rest = match location.split_once("://") {
        Some((_scheme, rest)) => match rest.find(['/', '?']) {
            Some(at) => &rest[at..],
            None => "/",
        },
        None => location,
    };
    let mut characters = rest.chars();
    match (characters.next(), characters.next()) {
        (Some('/'), Some('/' | '\\')) => "/",
        (Some('/'), _) => rest,
        _ => "/",
    }
}

#[cfg(test)]
mod refusal_redirect_tests {
    use super::carrying;

    #[test]
    fn a_bare_address_gains_a_query() {
        assert_eq!(
            carrying("http://izlek.sh/", "cycle", "link_tasks").as_deref(),
            Some("/?refusal=cycle&on=link_tasks")
        );
    }

    #[test]
    fn an_open_modal_stays_open() {
        assert_eq!(
            carrying("http://izlek.sh/?task=DZ-01", "cycle", "link_tasks").as_deref(),
            Some("/?task=DZ-01&refusal=cycle&on=link_tasks")
        );
    }

    #[test]
    fn a_second_refusal_replaces_the_first() {
        assert_eq!(
            carrying(
                "http://izlek.sh/?task=DZ-01&refusal=cycle&on=link_tasks",
                "not-found",
                "link_tasks"
            )
            .as_deref(),
            Some("/?task=DZ-01&refusal=not-found&on=link_tasks")
        );
    }

    // The Referer of a cross-origin post is the other site's address, and it
    // reaches this function as the Location. Izlek answers on its own ground or
    // not at all.
    #[test]
    fn another_site_cannot_be_redirected_to() {
        for elsewhere in [
            "https://elsewhere.example",
            "//elsewhere.example/steal",
            "/\\elsewhere.example/steal",
            "javascript:alert(1)",
            "",
        ] {
            assert_eq!(
                carrying(elsewhere, "cycle", "link_tasks").as_deref(),
                Some("/?refusal=cycle&on=link_tasks"),
                "{elsewhere} was not brought home"
            );
        }
        // An address with a path keeps the path — it is read as a path on this
        // site, which is the point: whatever the Referer claimed, the browser
        // is sent somewhere on Izlek.
        let carried = carrying(
            "http://elsewhere.example/steal?task=DZ-01",
            "cycle",
            "link_tasks",
        );
        assert_eq!(
            carried.as_deref(),
            Some("/steal?task=DZ-01&refusal=cycle&on=link_tasks")
        );
    }

    #[test]
    fn a_path_on_this_site_is_kept() {
        assert_eq!(
            carrying("/board?task=DZ-01", "cycle", "link_tasks").as_deref(),
            Some("/board?task=DZ-01&refusal=cycle&on=link_tasks")
        );
    }

    #[test]
    fn a_call_with_no_name_carries_nothing() {
        assert_eq!(carrying("http://izlek.sh/", "cycle", ""), None);
    }
}
