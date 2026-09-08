//! Every workspace member's face, proxied from im.
//!
//! `GET /avatar/{user_id}` answers with im's photo bytes for any member of
//! this workspace — not just the caller, whose own face is the common case.
//! The URL carries the member's `photo_version` as `?v=`, so a page may be
//! cached for months and the browser still refetches the day the face
//! changes: a matching `?v` is served `private, max-age=31536000, immutable`,
//! any other spelling revalidates, and the `ETag` names the row's version so
//! a revalidation ends in a bodyless 304. A browser without a session is
//! refused outright; an id outside the workspace, or im unreachable, is the
//! same 404 — the `<img>` hides itself and the initials beneath stand in.

use im_client::directory::DirectoryClient;
use topcoat::context::{Cx, try_app_context};
use topcoat::router::request::headers as request_headers;
use topcoat::router::{HeaderMap, HeaderValue, StatusCode, header, path_param, route};

use crate::server::{require_user, store};

path_param!(user_id);

fn not_found() -> (StatusCode, HeaderMap, Vec<u8>) {
    (StatusCode::NOT_FOUND, HeaderMap::new(), Vec::new())
}

/// The shared directory client `main.rs` registers on the router from the
/// `[oidc]` config: one HTTP pool, the app's Basic pair on every call.
pub fn directory(cx: &Cx) -> &DirectoryClient {
    try_app_context::<DirectoryClient>(cx)
        .expect("the directory client was registered on the router")
}

fn stamped_version(cx: &Cx) -> Option<u64> {
    let query = topcoat::router::request::uri(cx).query().unwrap_or("");
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == "v")
        .and_then(|(_, value)| value.parse::<u64>().ok())
}

/// `GET /avatar/{user_id}`: im's photo for the named member, or the same
/// not-found an unknown id would see — never a `403`, which would confirm
/// the id belongs to somebody.
#[route(GET "/avatar/{user_id}")]
async fn avatar(cx: &Cx) -> topcoat::Result<(StatusCode, HeaderMap, Vec<u8>)> {
    let target: &str = path_param::<UserId>(cx);

    let _viewer = match require_user(cx).await {
        Ok(user) => user,
        // An `<img>` has no page to carry a refusal on; 401 names the fix
        // the way the live channel's does.
        Err(_) => return Ok((StatusCode::UNAUTHORIZED, HeaderMap::new(), Vec::new())),
    };
    let row = store(cx).user(target).await?;
    let Some(row) = row else {
        return Ok(not_found());
    };
    let Some(sub) = row.oidc_sub.clone() else {
        return Ok(not_found());
    };
    let Ok((bytes, mime)) = directory(cx).photo(&sub).await else {
        return Ok(not_found());
    };

    // The row's version IS the version this URL names: the directory keeps
    // it current (live stream, and the beat behind it), so a stale face
    // means a stale row, never a lie about freshness.
    let etag = format!("\"p{}\"", row.photo_version);
    let mut headers = HeaderMap::new();
    headers.insert(header::ETAG, HeaderValue::from_str(&etag).unwrap());
    // A `?v` that agrees with the row means this exact face is pinned to a
    // URL that changes the day the face does, so the browser may keep it
    // for a year. Every other spelling — including no stamp at all —
    // revalidates. `private` because the route is gated and a shared proxy
    // must not answer a stranger from another account's entry.
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if stamped_version(cx) == Some(row.photo_version) {
            "private, max-age=31536000, immutable"
        } else {
            "private, no-cache"
        }),
    );
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&mime).unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );

    let if_none_match = request_headers(cx)
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok());
    if if_none_match == Some(etag.as_str()) {
        return Ok((StatusCode::NOT_MODIFIED, headers, Vec::new()));
    }
    Ok((StatusCode::OK, headers, bytes))
}
