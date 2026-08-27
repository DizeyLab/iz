//! Attachment upload and download, ported onto topcoat's router directly
//! rather than through `#[route]`'s JSON-answering server functions: a
//! multipart body and a byte stream are not things that codec is meant to
//! carry. Both routes read the session cookie through the same
//! `crate::server::require_user` every other route uses.
//!
//! Ported from `izlek-web/src/files.rs`.

use time::OffsetDateTime;
use topcoat::context::Cx;
use topcoat::router::content::multipart::Multipart;
use topcoat::router::{HeaderMap, HeaderValue, StatusCode, header, path_param, route};

use izlek_core::store::{NewAttachment, Store, User};

use crate::server::{Refusal, accounts, require_user};

path_param!(id);

/// The task, if this person's workspace is the one holding it. A task in
/// another workspace is not found rather than forbidden, same as
/// `izlek-web/src/detail.rs`'s `guard::task_of`.
async fn task_of(store: &dyn Store, user: &User, task_id: &str) -> Result<(), Refusal> {
    match store.task(task_id).await {
        Ok(Some(facts)) if facts.workspace_id == user.workspace_id => Ok(()),
        Ok(_) => Err(Refusal::NotFound),
        Err(error) => {
            eprintln!("store error: {error}");
            Err(Refusal::Unavailable)
        }
    }
}

/// A 303 to `location`, with no body.
fn redirect_to(location: &str) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut headers = HeaderMap::new();
    if let Ok(value) = HeaderValue::from_str(location) {
        headers.insert(header::LOCATION, value);
    }
    (StatusCode::SEE_OTHER, headers, Vec::new())
}

/// Where the browser lands once the upload is settled, success or refusal
/// alike — the task detail modal it was posted from, reopened.
fn back_to(task_id: &str, refusal: Option<Refusal>) -> (StatusCode, HeaderMap, Vec<u8>) {
    let location = match refusal {
        Some(refusal) => format!("/?task={task_id}&refusal={}&on=upload_file", refusal.code()),
        None => format!("/?task={task_id}"),
    };
    redirect_to(&location)
}

/// The same redirect, for the refusals that land before a `task_id` is even
/// known to carry in the query.
fn home(refusal: Refusal) -> (StatusCode, HeaderMap, Vec<u8>) {
    redirect_to(&format!("/?refusal={}&on=upload_file", refusal.code()))
}

/// A basename, stripped of anything that could make it look like a path or
/// carry a control character. What a browser calls a file is a label on a
/// row, never a place to write bytes.
fn label_of(file_name: &str) -> String {
    let base = file_name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(file_name);
    base.chars().filter(|c| !c.is_control()).collect()
}

/// What the bytes are, decided from the bytes themselves — never from the
/// part's `content_type()`, which is whatever the browser felt like sending.
fn sniff(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        "image/png"
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF8") {
        "image/gif"
    } else if bytes.starts_with(b"%PDF-") {
        "application/pdf"
    } else if bytes.starts_with(b"PK\x03\x04") {
        "application/zip"
    } else if bytes.starts_with(&[0x1F, 0x8B]) {
        "application/gzip"
    } else if std::str::from_utf8(bytes).is_ok() {
        "text/plain"
    } else {
        "application/octet-stream"
    }
}

/// The `Content-Disposition` header for one download: always `attachment`, so
/// an uploaded HTML or SVG file is offered to save rather than run on Izlek's
/// origin. The ASCII fallback keeps only characters no quoting scheme could
/// turn into a delimiter or a control character; `filename*` carries the real
/// name, percent-encoded, for browsers that read it.
fn disposition_of(file_name: &str) -> String {
    let ascii: String = file_name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' })
        .collect();
    let mut encoded = String::new();
    for &byte in file_name.as_bytes() {
        let c = byte as char;
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '~' | '-') {
            encoded.push(c);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{encoded}")
}

fn not_found() -> (StatusCode, HeaderMap, Vec<u8>) {
    (StatusCode::NOT_FOUND, HeaderMap::new(), Vec::new())
}

/// Takes one file onto a task. The route this hangs off already caps the
/// whole request body at the widest limit any workspace could set
/// ([`crate::settings::WIDEST_ATTACHMENT_MB`], registered in `main.rs`); this handler enforces
/// the workspace's own, usually narrower, limit while the bytes are still
/// arriving rather than after they have all landed.
///
/// Fields arrive in request order: `task_id` before `file`, so the task and
/// the workspace's limits are known before a byte of the file is kept.
#[route(POST "/files")]
async fn upload(cx: &Cx, mut multipart: Multipart) -> topcoat::Result<(StatusCode, HeaderMap, Vec<u8>)> {
    let user = match require_user(cx).await {
        Ok(user) => user,
        Err(refusal) => return Ok(home(refusal)),
    };
    if !user.role.can_comment() {
        return Ok(home(Refusal::Forbidden));
    }

    let mut task_id: Option<String> = None;
    let mut comment_id: Option<String> = None;
    let mut file_name: Option<String> = None;
    let mut bytes: Option<Vec<u8>> = None;

    let store = accounts(cx).store().clone();

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(_) => return Ok(home(Refusal::NoFile)),
        };
        match field.name() {
            Some("task_id") => {
                task_id = field.text().await.ok();
            }
            Some("comment_id") => {
                comment_id = field.text().await.ok();
            }
            Some("file") => {
                let Some(name) = field.file_name().map(str::to_string) else {
                    continue;
                };
                if name.is_empty() {
                    continue;
                }

                let Some(task_id) = task_id.as_deref() else {
                    return Ok(home(Refusal::NotFound));
                };
                if let Err(refusal) = task_of(store.as_ref(), &user, task_id).await {
                    return Ok(back_to(task_id, Some(refusal)));
                }
                let Ok(Some(workspace)) = store.workspace().await else {
                    return Ok(back_to(task_id, Some(Refusal::NotFound)));
                };
                let extension = name.rsplit_once('.').map(|(_, ext)| ext.to_lowercase());
                let allowed = workspace.allowed_file_types.is_empty()
                    || extension
                        .as_deref()
                        .is_some_and(|ext| workspace.allowed_file_types.iter().any(|a| a == ext));
                if !allowed {
                    return Ok(back_to(task_id, Some(Refusal::FileTypeNotAllowed)));
                }

                let limit = workspace.attachment_limit_bytes;
                let mut field = field;
                let mut collected = Vec::new();
                loop {
                    match field.chunk().await {
                        Ok(Some(chunk)) => {
                            if (collected.len() + chunk.len()) as u64 > limit {
                                return Ok(back_to(task_id, Some(Refusal::FileTooBig)));
                            }
                            collected.extend_from_slice(&chunk);
                        }
                        Ok(None) => break,
                        Err(_) => return Ok(back_to(task_id, Some(Refusal::FileTooBig))),
                    }
                }

                file_name = Some(name);
                bytes = Some(collected);
            }
            _ => {}
        }
    }

    let Some(task_id) = task_id else {
        return Ok(home(Refusal::NotFound));
    };
    let Some(file_name) = file_name else {
        return Ok(back_to(&task_id, Some(Refusal::NoFile)));
    };
    let bytes = bytes.unwrap_or_default();

    // A `comment_id` only travels if it names a comment that is actually on
    // this task; anything else is dropped rather than refused, since the
    // upload itself is still perfectly good without it.
    let comment_id = match comment_id {
        Some(id) => {
            let on_task = store
                .comments_for_task(&task_id)
                .await
                .map(|comments| comments.iter().any(|c| c.id == id))
                .unwrap_or(false);
            on_task.then_some(id)
        }
        None => None,
    };

    let label = label_of(&file_name);
    let mime_type = sniff(&bytes);

    let added = store
        .add_attachment(NewAttachment {
            task_id: &task_id,
            comment_id: comment_id.as_deref(),
            file_name: &label,
            mime_type,
            bytes,
            uploaded_by: &user.id,
            at: OffsetDateTime::now_utc(),
        })
        .await;

    Ok(match added {
        Ok(_) => back_to(&task_id, None),
        Err(_) => back_to(&task_id, Some(Refusal::NotFound)),
    })
}

/// Serves one attachment's bytes, or the same not-found a stranger to the
/// task would see for a task that does not exist — never a `403`, which would
/// confirm the id belongs to someone else's workspace.
#[route(GET "/files/{id}")]
async fn download(cx: &Cx) -> topcoat::Result<(StatusCode, HeaderMap, Vec<u8>)> {
    let id: &str = path_param::<Id>(cx);

    let user = match require_user(cx).await {
        Ok(user) => user,
        Err(refusal) => return Ok(home(refusal)),
    };

    let store = accounts(cx).store().clone();
    let Ok(Some(row)) = store.attachment(id).await else {
        return Ok(not_found());
    };
    if task_of(store.as_ref(), &user, &row.task_id).await.is_err() {
        return Ok(not_found());
    }
    let Ok(Some(bytes)) = store.attachment_bytes(id).await else {
        return Ok(not_found());
    };

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&row.mime_type).unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&disposition_of(&row.file_name)).unwrap_or(HeaderValue::from_static("attachment")),
    );
    Ok((StatusCode::OK, headers, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_the_magic_numbers() {
        assert_eq!(sniff(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A]), "image/png");
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), "image/jpeg");
        assert_eq!(sniff(b"hello, this is plainly text"), "text/plain");
        assert_eq!(sniff(&[0x00, 0x01, 0xFE, 0xFF, 0x02]), "application/octet-stream");
    }

    #[test]
    fn labels_strip_the_path_and_the_control_characters() {
        assert_eq!(label_of("../../etc/passwd"), "passwd");
        assert_eq!(label_of(r"a\b.txt"), "b.txt");
        assert_eq!(label_of("name\r\nwith\tcontrol.txt"), "namewithcontrol.txt");
    }

    #[test]
    fn disposition_carries_no_quote_or_control_character() {
        let header = disposition_of("a\"b\r\nX: y.txt");
        assert!(!header.contains('\r'));
        assert!(!header.contains('\n'));
        assert_eq!(
            header,
            "attachment; filename=\"a_b__X__y.txt\"; filename*=UTF-8''a%22b%0D%0AX%3A%20y.txt"
        );
    }

    #[test]
    fn disposition_of_a_traversal_name_keeps_no_path_meaning() {
        let header = disposition_of("../../etc/passwd");
        assert_eq!(
            header,
            "attachment; filename=\".._.._etc_passwd\"; filename*=UTF-8''..%2F..%2Fetc%2Fpasswd"
        );
    }

    #[test]
    fn disposition_of_a_non_ascii_name_falls_back_and_percent_encodes() {
        let header = disposition_of("résumé.pdf");
        assert_eq!(
            header,
            "attachment; filename=\"r_sum_.pdf\"; filename*=UTF-8''r%C3%A9sum%C3%A9.pdf"
        );
    }
}
