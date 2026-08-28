//! Profile photo upload, removal and serving. Sibling to `files.rs`, whose
//! multipart parsing and mime-sniffing it reuses rather than re-implements.

use topcoat::context::Cx;
use topcoat::router::content::multipart::Multipart;
use topcoat::router::request::headers as request_headers;
use topcoat::router::{HeaderMap, HeaderValue, StatusCode, header, path_param, route};

use crate::files::sniff;
use crate::server::{Refusal, accounts, require_user};
use crate::settings::saved_or_refused;

path_param!(user_id);

fn not_found() -> (StatusCode, HeaderMap, Vec<u8>) {
    (StatusCode::NOT_FOUND, HeaderMap::new(), Vec::new())
}

/// Sets the signed-in person's own photo. Nobody else's — the id comes from
/// the session, never from the form.
#[route(POST "/api/profile_photo")]
async fn upload(
    cx: &Cx,
    mut multipart: Multipart,
) -> topcoat::Result<(StatusCode, HeaderMap, Vec<u8>)> {
    let user = match require_user(cx).await {
        Ok(user) => user,
        Err(refusal) => return Ok(saved_or_refused("profile_photo", Some(refusal))),
    };
    let store = accounts(cx).store().clone();
    let Ok(Some(workspace)) = store.workspace().await else {
        return Ok(saved_or_refused(
            "profile_photo",
            Some(Refusal::Unavailable),
        ));
    };
    let limit = workspace.photo_limit_bytes;

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => return Ok(saved_or_refused("profile_photo", Some(Refusal::NoFile))),
            Err(_) => return Ok(saved_or_refused("profile_photo", Some(Refusal::NoFile))),
        };
        if field.file_name().is_none() {
            continue;
        }
        let mut field = field;
        let mut collected = Vec::new();
        loop {
            match field.chunk().await {
                Ok(Some(chunk)) => {
                    if (collected.len() + chunk.len()) as u64 > limit {
                        return Ok(saved_or_refused("profile_photo", Some(Refusal::FileTooBig)));
                    }
                    collected.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(_) => return Ok(saved_or_refused("profile_photo", Some(Refusal::FileTooBig))),
            }
        }
        let mime = sniff(&collected);
        if !matches!(
            mime,
            "image/png" | "image/jpeg" | "image/gif" | "image/webp" | "image/avif"
        ) {
            return Ok(saved_or_refused("profile_photo", Some(Refusal::NotAnImage)));
        }
        return Ok(match store.set_photo(&user.id, &collected, mime).await {
            Ok(()) => saved_or_refused("profile_photo", None),
            Err(_) => saved_or_refused("profile_photo", Some(Refusal::Unavailable)),
        });
    }
}

/// Clears the signed-in person's own photo.
#[route(POST "/api/delete_profile_photo")]
async fn delete(cx: &Cx) -> topcoat::Result<(StatusCode, HeaderMap, Vec<u8>)> {
    let user = match require_user(cx).await {
        Ok(user) => user,
        Err(refusal) => return Ok(saved_or_refused("delete_profile_photo", Some(refusal))),
    };
    let store = accounts(cx).store().clone();
    Ok(match store.clear_photo(&user.id).await {
        Ok(()) => saved_or_refused("delete_profile_photo", None),
        Err(_) => saved_or_refused("delete_profile_photo", Some(Refusal::Unavailable)),
    })
}

/// A cheap, non-cryptographic hash — good enough for an `ETag` on bytes only
/// this server ever writes.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Serves one person's photo, or the not-found a stranger to their workspace
/// would see for a person with none at all — never `403`, same reasoning as
/// `files.rs`'s `download`.
#[route(GET "/photo/{user_id}")]
async fn serve(cx: &Cx) -> topcoat::Result<(StatusCode, HeaderMap, Vec<u8>)> {
    let user_id: &str = path_param::<UserId>(cx);

    let asking = match require_user(cx).await {
        Ok(user) => user,
        Err(_) => return Ok(not_found()),
    };

    let store = accounts(cx).store().clone();
    let Ok(Some(target)) = store.user(user_id).await else {
        return Ok(not_found());
    };
    if target.workspace_id != asking.workspace_id || !target.has_photo {
        return Ok(not_found());
    }
    let Ok(Some((bytes, mime))) = store.photo(user_id).await else {
        return Ok(not_found());
    };

    let etag = format!("\"{:x}\"", fnv1a(&bytes));
    let mut headers = HeaderMap::new();
    headers.insert(header::ETAG, HeaderValue::from_str(&etag).unwrap());

    let if_none_match = request_headers(cx)
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok());
    if if_none_match == Some(etag.as_str()) {
        return Ok((StatusCode::NOT_MODIFIED, headers, Vec::new()));
    }

    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&mime)
            .unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    Ok((StatusCode::OK, headers, bytes))
}
