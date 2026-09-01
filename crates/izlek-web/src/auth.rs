//! The account surface: claiming the workspace, first sign-in from an invited
//! link, signing in and out, and changing a password.
//!
//! Ported from `izlek-web/src/auth.rs`. Every mutating call answers a browser
//! without script the same way: a 303 back to wherever the form was posted
//! from, with the refusal (if any) serialized as the body so
//! [`crate::server::carry_refusal_on_redirect`] can carry it onto that
//! redirect's query.

use izlek_core::accounts::SESSION_LIFETIME;
use izlek_core::detail::ActivityKind;
use serde::{Deserialize, Serialize};
use topcoat::Result;
use topcoat::context::Cx;
use topcoat::router::content::{Form, Json};
use topcoat::router::request::headers;
use topcoat::router::response::{IntoResponse, Response};
use topcoat::router::{HeaderName, StatusCode, header, route};

use crate::server::{
    Refusal, accounts, back_to, clear_session_cookie, client_label, mail, presented_session,
    require_admin, require_user, set_session_cookie,
};
use crate::settings::encode_q;

/// The workspace has exactly one name and no screen that sets it, so it is a
/// constant rather than a field nobody was shown.
pub const WORKSPACE_NAME: &str = "İzlek";

/// The name and address an invitation was made out to.
#[derive(Clone, Debug, Serialize)]
pub struct Invited {
    pub display_name: String,
    pub email: String,
    /// The name of whoever made the account. None only if that person's own
    /// account has since been removed.
    pub invited_by: Option<String>,
}

/// Who an invitation was made out to, looked up by the link's own token — not
/// by the mail it was sent to, which the browser holding the link never sees.
///
/// Shared by the `/api/invitation` route and the `/join/{token}` page: both
/// answer the same question, one for a hydrated caller and one to render.
pub async fn invited_by_token(cx: &Cx, token: &str) -> Result<Option<Invited>> {
    use izlek_core::auth::hash_token;
    use time::OffsetDateTime;

    let store = accounts(cx).store().clone();
    let digest = hash_token(token);
    let Some(link) = store.signin_link_by_hash(&digest).await? else {
        return Ok(None);
    };
    if !link.is_usable(OffsetDateTime::now_utc()) {
        return Ok(None);
    }
    let Some(user) = store.user(&link.user_id).await? else {
        return Ok(None);
    };
    let invited_by = match &user.invited_by {
        Some(admin_id) => store.user(admin_id).await?.map(|admin| admin.display_name),
        None => None,
    };
    Ok(Some(Invited {
        display_name: user.display_name,
        email: user.email,
        invited_by,
    }))
}

/// A 303 to [`back_to`], carrying `refusal` as the body for
/// `carry_refusal_on_redirect` to read and copy onto the query.
type Redirect = Result<(StatusCode, [(HeaderName, String); 1], Json<Option<Refusal>>)>;

fn redirect(cx: &Cx, refusal: Option<Refusal>) -> Redirect {
    Ok((
        StatusCode::SEE_OTHER,
        [(header::LOCATION, back_to(cx, "/"))],
        Json(refusal),
    ))
}

#[derive(Deserialize)]
struct ClaimWorkspaceForm {
    display_name: String,
    email: String,
    password: String,
}

/// The first account. It becomes the admin and owns the workspace.
#[route(POST "/api/claim_workspace")]
async fn claim_workspace(cx: &Cx, Form(input): Form<ClaimWorkspaceForm>) -> Redirect {
    match accounts(cx)
        .claim_workspace(
            WORKSPACE_NAME,
            &input.email,
            &input.display_name,
            &input.password,
        )
        .await
    {
        Ok((_workspace, signed_in)) => {
            set_session_cookie(cx, signed_in.session_token.expose(), SESSION_LIFETIME);
            let _ = accounts(cx)
                .store()
                .record_event(
                    Some(&signed_in.user.id),
                    &ActivityKind::WorkspaceClaimed,
                    WORKSPACE_NAME,
                    time::OffsetDateTime::now_utc(),
                )
                .await;
            redirect(cx, None)
        }
        Err(error) => redirect(cx, Some(error.into())),
    }
}

#[derive(Deserialize)]
struct SignInForm {
    email: String,
    password: String,
}

/// Signing in. Answers the same whether the address is unknown, has no
/// password yet, or the password is wrong.
#[route(POST "/api/sign_in")]
async fn sign_in(cx: &Cx, Form(input): Form<SignInForm>) -> Redirect {
    match accounts(cx)
        .sign_in(&input.email, &input.password, &client_label(cx))
        .await
    {
        Ok(signed_in) => {
            set_session_cookie(cx, signed_in.session_token.expose(), SESSION_LIFETIME);
            let _ = accounts(cx)
                .store()
                .record_event(
                    Some(&signed_in.user.id),
                    &ActivityKind::SignedIn,
                    &input.email,
                    time::OffsetDateTime::now_utc(),
                )
                .await;
            redirect(cx, None)
        }
        Err(error) => {
            if !matches!(error, izlek_core::accounts::AccountError::RateLimited) {
                let _ = accounts(cx)
                    .store()
                    .record_event(
                        None,
                        &ActivityKind::SignInFailed,
                        &input.email,
                        time::OffsetDateTime::now_utc(),
                    )
                    .await;
            }
            redirect(cx, Some(error.into()))
        }
    }
}

/// Ends this browser's session. Other browsers keep theirs. Always lands home
/// — wherever the sign-out button was pressed, a signed-out browser belongs
/// at the front door.
#[route(POST "/api/sign_out")]
async fn sign_out(cx: &Cx) -> Result<(StatusCode, [(HeaderName, &'static str); 1])> {
    if let Some(presented) = presented_session(cx) {
        if let Ok(user) = require_user(cx).await {
            let _ = accounts(cx)
                .store()
                .record_event(
                    Some(&user.id),
                    &ActivityKind::SignedOut,
                    "",
                    time::OffsetDateTime::now_utc(),
                )
                .await;
        }
        let _ = accounts(cx).sign_out(&presented).await;
    }
    clear_session_cookie(cx);
    Ok((StatusCode::SEE_OTHER, [(header::LOCATION, "/")]))
}

#[derive(Deserialize)]
struct RedeemLinkForm {
    token: String,
    password: String,
}

/// The invited member's first sign-in: they pick their own password. The
/// admin can neither read nor set it.
#[route(POST "/api/redeem_link")]
async fn redeem_link(cx: &Cx, Form(input): Form<RedeemLinkForm>) -> Redirect {
    match accounts(cx)
        .redeem_signin_link(&input.token, &input.password, &client_label(cx))
        .await
    {
        Ok(signed_in) => {
            set_session_cookie(cx, signed_in.session_token.expose(), SESSION_LIFETIME);
            let _ = accounts(cx)
                .store()
                .record_event(
                    Some(&signed_in.user.id),
                    &ActivityKind::Joined,
                    &signed_in.user.email,
                    time::OffsetDateTime::now_utc(),
                )
                .await;
            // Redeemed; the referring `/join/{token}` now names a spent
            // link, which would show "no longer works" to someone who just
            // signed in, so land on the board itself instead.
            Ok((
                StatusCode::SEE_OTHER,
                [(header::LOCATION, "/".to_string())],
                Json(None),
            ))
        }
        Err(error) => redirect(cx, Some(error.into())),
    }
}

#[derive(Deserialize)]
struct ChangePasswordForm {
    current: String,
    new: String,
}

/// Changes the password and signs the other devices out, as the pane
/// promises. The browser that asked gets a fresh cookie.
#[route(POST "/api/change_password")]
async fn change_password(cx: &Cx, Form(input): Form<ChangePasswordForm>) -> Redirect {
    let user = match require_user(cx).await {
        Ok(user) => user,
        Err(refusal) => return redirect(cx, Some(refusal)),
    };
    match accounts(cx)
        .change_password(&user.id, &input.current, &input.new, &client_label(cx))
        .await
    {
        Ok(signed_in) => {
            set_session_cookie(cx, signed_in.session_token.expose(), SESSION_LIFETIME);
            let _ = accounts(cx)
                .store()
                .record_event(
                    Some(&signed_in.user.id),
                    &ActivityKind::PasswordChanged,
                    "",
                    time::OffsetDateTime::now_utc(),
                )
                .await;
            redirect(cx, None)
        }
        Err(error) => redirect(cx, Some(error.into())),
    }
}

#[derive(Deserialize)]
struct InviteMemberForm {
    email: String,
    display_name: String,
    role: izlek_core::Role,
}

/// Admin creates an account with a name and an address and no password. The
/// first-sign-in link goes to that address by mail, never to the browser.
///
/// A caller with script reads the address straight off the JSON body, same
/// as always. A plain form post has no such reader, so — same as
/// `resend_link` (`izlek-web/src/settings.rs`) — it lands back on Settings
/// instead: `mailed=<address>` on success, `refusal=<code>&on=invite_member`
/// otherwise.
#[route(POST "/api/invite_member")]
async fn invite_member(cx: &Cx, Form(input): Form<InviteMemberForm>) -> Result<Response> {
    let has_referer = headers(cx).contains_key(header::REFERER);
    let admin = match require_admin(cx).await {
        Ok(admin) => admin,
        Err(refusal) => return invite_answer(cx, has_referer, Err(refusal)),
    };
    match accounts(cx)
        .invite(&admin, &input.email, &input.display_name, input.role)
        .await
    {
        Ok(made) => {
            mail(cx).after_invite();
            let _ = accounts(cx)
                .store()
                .record_event(
                    Some(&admin.id),
                    &ActivityKind::Invited,
                    &input.email,
                    time::OffsetDateTime::now_utc(),
                )
                .await;
            invite_answer(cx, has_referer, Ok(made.user.email))
        }
        Err(error) => invite_answer(cx, has_referer, Err(error.into())),
    }
}

/// The address mailed, or the refusal, either as JSON for a caller with
/// script or a 303 back to Settings for a browser form post.
fn invite_answer(
    cx: &Cx,
    has_referer: bool,
    outcome: std::result::Result<String, Refusal>,
) -> Result<Response> {
    if !has_referer {
        return Json(outcome).into_response(cx);
    }
    let location = match outcome {
        Ok(address) => format!("/settings?mailed={}&section=members", encode_q(&address)),
        Err(refusal) => format!(
            "/settings?refusal={}&on=invite_member&section=members",
            refusal.code()
        ),
    };
    (StatusCode::SEE_OTHER, [(header::LOCATION, location)], ()).into_response(cx)
}

#[derive(Deserialize)]
struct InvitationForm {
    token: String,
}

/// Who an invitation was made out to, for the "signing in as" line. Only the
/// holder of the link can ask, and the link is a 128-bit secret, so answering
/// is safe.
#[route(POST "/api/invitation")]
async fn invitation(cx: &Cx, Form(input): Form<InvitationForm>) -> Result<Json<Option<Invited>>> {
    Ok(Json(invited_by_token(cx, &input.token).await?))
}
