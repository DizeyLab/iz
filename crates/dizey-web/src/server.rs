//! Server-side plumbing for the auth surface: the session cookie, the person
//! behind the current request, and the role guards.
//!
//! Every guard here is the real one. The UI hides what a role may not do, but
//! the answer that matters is the one given in this module, on the server.

use axum::http::HeaderValue;
use axum::http::header::{COOKIE, SET_COOKIE};
use axum::http::request::Parts;
use dizey_core::accounts::{AccountError, Accounts};
use dizey_core::store::User;
use leptos::prelude::*;
use leptos_axum::ResponseOptions;

/// The session cookie's name. One cookie, one browser, one session row.
pub const SESSION_COOKIE: &str = "dizey_session";

/// The workspace's account service, put into context by the router.
pub fn accounts() -> Accounts {
    expect_context::<Accounts>()
}

/// The cookie value this request presented, if it presented one.
pub fn presented_session() -> Option<String> {
    let parts = use_context::<Parts>()?;
    for header in parts.headers.get_all(COOKIE) {
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
/// only trusted because Dizey is meant to sit behind one; the address bucket is
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
pub fn router(accounts: Accounts, leptos_options: LeptosOptions) -> axum::Router {
    use axum::Router;
    use leptos_axum::{LeptosRoutes, generate_route_list};

    let routes = generate_route_list(crate::app::App);
    Router::new()
        .route("/healthz", axum::routing::get(|| async { "ok" }))
        .leptos_routes_with_context(
            &leptos_options,
            routes,
            {
                // Provided here *and* to the server-function handler, which
                // `leptos_routes_with_context` registers with the same closure.
                let accounts = accounts.clone();
                move || provide_context(accounts.clone())
            },
            {
                let leptos_options = leptos_options.clone();
                move || crate::app::shell(leptos_options.clone())
            },
        )
        .fallback(leptos_axum::file_and_error_handler(crate::app::shell))
        .with_state(leptos_options)
}
