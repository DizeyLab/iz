//! Profile photo upload, removal and serving. Sibling to `files.rs`, whose
//! multipart parsing and mime-sniffing it reuses rather than re-implements.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use time::OffsetDateTime;
use topcoat::context::{Cx, try_app_context};
use topcoat::router::content::multipart::Multipart;
use topcoat::router::request::headers as request_headers;
use topcoat::router::{HeaderMap, HeaderValue, StatusCode, header, path_param, route};

use izlek_core::store::sniff::sniff;
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
            Ok(()) => {
                // The bytes changed, so the URL every avatar renders has to
                // change with them — bump before the answer goes out.
                if let Some(stamps) = try_app_context::<PhotoStamps>(cx) {
                    stamps.bump(&user.id);
                }
                let _ = store
                    .record_event(
                        Some(&user.id),
                        &izlek_core::detail::ActivityKind::Other("photo_saved".to_string()),
                        "",
                        OffsetDateTime::now_utc(),
                    )
                    .await;
                saved_or_refused("profile_photo", None)
            }
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
        Ok(()) => {
            let _ = store
                .record_event(
                    Some(&user.id),
                    &izlek_core::detail::ActivityKind::Other("photo_removed".to_string()),
                    "",
                    OffsetDateTime::now_utc(),
                )
                .await;
            saved_or_refused("delete_profile_photo", None)
        }
        Err(_) => saved_or_refused("delete_profile_photo", Some(Refusal::Unavailable)),
    })
}

/// Photo URL version stamps, in process memory. The `user` row carries no
/// photo-updated moment and a schema column for cache-busting is out of
/// proportion, so the stamp lives here: `upload` bumps it when the bytes
/// change, and every avatar render reads it back. A photo whose bytes this
/// process never saw change — one uploaded by an earlier process — stamps
/// at the process start, which is still a URL no browser has fetched, so
/// it is re-downloaded exactly once per restart and cached from then on.
#[derive(Clone, Default)]
pub struct PhotoStamps(Arc<Mutex<HashMap<String, i64>>>);

/// Unix microseconds at first use: the stamp for every photo whose bytes
/// this process never saw change. Stable for the process's lifetime, and
/// later than any stamp an earlier process emitted, so no pre-restart URL
/// survives a restart.
static PROCESS_START: LazyLock<i64> =
    LazyLock::new(|| (OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000) as i64);

impl PhotoStamps {
    fn stamp(&self, user_id: &str) -> i64 {
        self.0
            .lock()
            .unwrap()
            .get(user_id)
            .copied()
            .unwrap_or(*PROCESS_START)
    }

    fn bump(&self, user_id: &str) {
        let now = (OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000) as i64;
        let mut stamps = self.0.lock().unwrap();
        // A write always moves the URL: never emit a stamp this user has
        // already rendered with, even when two writes land inside the same
        // microsecond.
        let stamp = stamps.get(user_id).copied().unwrap_or(*PROCESS_START);
        stamps.insert(user_id.to_string(), now.max(stamp + 1));
    }
}

/// The stamp an avatar's photo URL carries. A router built without
/// `PhotoStamps` — the test router does this — falls back to the process
/// start, which is still a URL no browser has fetched before.
pub(crate) fn photo_stamp(cx: &Cx, user_id: &str) -> i64 {
    match try_app_context::<PhotoStamps>(cx) {
        Some(stamps) => stamps.stamp(user_id),
        None => *PROCESS_START,
    }
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
    // The stamp in the URL is the other half of this caching: `upload`
    // bumps it whenever the bytes change, so a year of `immutable` never
    // shows an old photo — a changed photo is a changed URL. `private`
    // because the route is session-gated and a shared proxy cache would
    // otherwise answer a stranger from another workspace's entry.
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=31536000, immutable"),
    );

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
