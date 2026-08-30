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
use topcoat::router::request::headers as request_headers;
use topcoat::router::{
    HeaderMap, HeaderValue, StatusCode, header, path_param, query_params, route,
};

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
        Some(refusal) => {
            format!("/?task={task_id}&tab=files&refusal={}&on=upload_file", refusal.code())
        }
        None => format!("/?task={task_id}&tab=files"),
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
    let base = file_name.rsplit(['/', '\\']).next().unwrap_or(file_name);
    base.chars().filter(|c| !c.is_control()).collect()
}

/// What the bytes are, decided from the bytes themselves — never from the
/// part's `content_type()`, which is whatever the browser felt like sending.
pub(crate) fn sniff(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        "image/png"
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF8") {
        "image/gif"
    } else if bytes.starts_with(b"%PDF-") {
        "application/pdf"
    } else if bytes.starts_with(b"PK\x03\x04") {
        // Every OOXML and OpenDocument file is a zip; what kind it is lives in
        // the entry names, which a zip stores uncompressed. `xl/workbook.xml`
        // is the one part an xlsx cannot be without, and an ods declares its
        // type in the `mimetype` entry the format requires be stored first.
        if contains(bytes, b"xl/workbook.xml") {
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
        } else if contains(bytes, b"opendocument.spreadsheet") {
            "application/vnd.oasis.opendocument.spreadsheet"
        } else {
            "application/zip"
        }
    } else if bytes.starts_with(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]) {
        // The pre-2007 OLE compound file .xls, .doc and .ppt all share. Its
        // directory holds stream names in UTF-16, and only a workbook carries
        // one called `Workbook` — a .doc or .ppt stays an unnamed container.
        if contains(bytes, b"W\0o\0r\0k\0b\0o\0o\0k\0") {
            "application/vnd.ms-excel"
        } else {
            "application/x-ole-storage"
        }
    } else if bytes.starts_with(&[0x1F, 0x8B]) {
        "application/gzip"
    } else if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" && &bytes[8..12] == b"avif" {
        "image/avif"
    } else if bytes.len() >= 12
        && &bytes[4..8] == b"ftyp"
        && matches!(
            &bytes[8..12],
            b"heic" | b"heix" | b"hevc" | b"heif" | b"mif1" | b"msf1"
        )
    {
        // Apple's HEIC container reuses the ISO-BMFF `ftyp` box video shares;
        // the brand at bytes 8..12 is what tells a photo from a video apart.
        "image/heic"
    } else if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" && &bytes[8..12] == b"qt  " {
        "video/quicktime"
    } else if bytes.len() >= 8 && &bytes[4..8] == b"ftyp" {
        "video/mp4"
    } else if bytes.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        "video/webm"
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WAVE" {
        "audio/wav"
    } else if bytes.starts_with(b"fLaC") {
        "audio/flac"
    } else if bytes.starts_with(b"OggS") {
        "audio/ogg"
    } else if bytes.starts_with(b"ID3")
        || bytes.starts_with(&[0xFF, 0xFB])
        || bytes.starts_with(&[0xFF, 0xF3])
        || bytes.starts_with(&[0xFF, 0xF2])
    {
        "audio/mpeg"
    } else if std::str::from_utf8(bytes).is_ok() {
        "text/plain"
    } else {
        "application/octet-stream"
    }
}

/// Whether `haystack` holds `needle` anywhere in it. Used on zip bytes, where
/// the entry names sit in the clear even when the entries themselves do not.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// The `Content-Disposition` header for one download. `inline` is only ever
/// true for a mime type a browser renders on its own ([`renders_inline`]) and
/// the caller did not ask for a forced download (`?dl=1`) — an uploaded HTML
/// or SVG file stays `attachment` either way, so it is offered to save rather
/// than run on Izlek's origin. The ASCII fallback keeps only characters no
/// quoting scheme could turn into a delimiter or a control character;
/// `filename*` carries the real name, percent-encoded, for browsers that read
/// it.
fn disposition_of(file_name: &str, inline: bool) -> String {
    let kind = if inline { "inline" } else { "attachment" };
    let ascii: String = file_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
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
    format!("{kind}; filename=\"{ascii}\"; filename*=UTF-8''{encoded}")
}

/// Whether the browser renders this stored mime type on its own — image,
/// video, audio, PDF or plain text — rather than only offering to save it.
/// Trusts the stored mime alone; nothing here sniffs bytes a second time.
/// `image/heic` is excluded: no browser engine decodes it, so it stays a
/// download link rather than a broken `<img>`.
fn renders_inline(mime_type: &str) -> bool {
    mime_type != "image/heic"
        && (mime_type.starts_with("image/")
            || mime_type.starts_with("video/")
            || mime_type.starts_with("audio/")
            || mime_type == "application/pdf"
            || mime_type == "text/plain")
}

/// The element the in-app viewer opens one attachment in, decided from its
/// stored mime type alone. `None` means there is no overlay for it: a
/// filename click downloads like any other attachment. Plain text renders
/// inline in the browser ([`renders_inline`]) but has no viewer element of
/// its own, so it is not a [`ViewerKind`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ViewerKind {
    Image,
    Video,
    Audio,
    Pdf,
    /// A spreadsheet Izlek reads itself and lays out as a table — no browser
    /// renders one, so this is the only viewer whose bytes are parsed here
    /// rather than handed to an element.
    Sheet,
}

/// The stored mime types [`crate::sheet`] opens: the two Excel workbook
/// formats and OpenDocument's. A file that sniffs as one of these still has
/// to parse before a table is drawn; [`ViewerKind::Sheet`] only says to try.
pub(crate) fn is_spreadsheet(mime_type: &str) -> bool {
    matches!(
        mime_type,
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
            | "application/vnd.ms-excel"
            | "application/vnd.oasis.opendocument.spreadsheet"
    )
}

pub(crate) fn viewer_kind(mime_type: &str) -> Option<ViewerKind> {
    if mime_type == "image/heic" {
        None
    } else if mime_type.starts_with("image/") {
        Some(ViewerKind::Image)
    } else if mime_type.starts_with("video/") {
        Some(ViewerKind::Video)
    } else if mime_type.starts_with("audio/") {
        Some(ViewerKind::Audio)
    } else if mime_type == "application/pdf" {
        Some(ViewerKind::Pdf)
    } else if is_spreadsheet(mime_type) {
        Some(ViewerKind::Sheet)
    } else {
        None
    }
}

fn not_found() -> (StatusCode, HeaderMap, Vec<u8>) {
    (StatusCode::NOT_FOUND, HeaderMap::new(), Vec::new())
}

/// A single-range `Range: bytes=...` request resolved against `total` bytes,
/// as `(start, end)` inclusive. `None` for anything this does not parse as
/// exactly one `bytes=` range (a multi-range header included) — the caller
/// falls back to a full `200` response, which is always a valid answer to a
/// `Range` request. `Some(None)` would be needless: an unsatisfiable range
/// (start at or past `total`, or an empty file) is reported as `Err(())` so
/// the caller can answer `416` instead of serving nonsense bytes.
fn parse_range(header: &str, total: u64) -> Option<Result<(u64, u64), ()>> {
    let spec = header.strip_prefix("bytes=")?;
    if spec.contains(',') || total == 0 {
        return None;
    }
    let (start, end) = spec.split_once('-')?;
    if start.is_empty() {
        // `bytes=-N`: the last N bytes.
        let suffix: u64 = end.parse().ok()?;
        if suffix == 0 {
            return Some(Err(()));
        }
        let start = total.saturating_sub(suffix);
        return Some(Ok((start, total - 1)));
    }
    let start: u64 = start.parse().ok()?;
    if start >= total {
        return Some(Err(()));
    }
    let end = if end.is_empty() {
        total - 1
    } else {
        end.parse::<u64>().ok()?.min(total - 1)
    };
    if end < start {
        return Some(Err(()));
    }
    Some(Ok((start, end)))
}

/// `?dl=1` forces `attachment` on a type that would otherwise render inline
/// — "download instead" stays an option even for a file the viewer can open.
#[query_params(error = not_found)]
struct DownloadQuery {
    dl: Option<String>,
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
async fn upload(
    cx: &Cx,
    mut multipart: Multipart,
) -> topcoat::Result<(StatusCode, HeaderMap, Vec<u8>)> {
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

    if added.is_ok() && comment_id.is_none() {
        let _ = store
            .record_activity(
                &task_id,
                Some(&user.id),
                &izlek_core::detail::ActivityKind::FileAdded,
                &label,
                OffsetDateTime::now_utc(),
            )
            .await;
    }

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

    let forced_download = query_params::<DownloadQuery>(cx)?.dl.is_some();
    let inline = !forced_download && renders_inline(&row.mime_type);

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&row.mime_type)
            .unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&disposition_of(&row.file_name, inline))
            .unwrap_or(HeaderValue::from_static("attachment")),
    );
    // Safari refuses to play a `<video>`/`<audio>` element without a `206`
    // reply to its own `Range` probe; every other engine loses instant seek
    // without one. Sent on every response, not only media — harmless either
    // way and one less type to special-case.
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));

    let total = bytes.len() as u64;
    let range = request_headers(cx)
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok());
    match range.and_then(|r| parse_range(r, total)) {
        Some(Ok((start, end))) => {
            let slice = bytes[start as usize..=end as usize].to_vec();
            headers.insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes {start}-{end}/{total}")).unwrap(),
            );
            Ok((StatusCode::PARTIAL_CONTENT, headers, slice))
        }
        Some(Err(())) => {
            headers.insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes */{total}")).unwrap(),
            );
            Ok((StatusCode::RANGE_NOT_SATISFIABLE, headers, Vec::new()))
        }
        None => Ok((StatusCode::OK, headers, bytes)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_the_magic_numbers() {
        assert_eq!(sniff(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A]), "image/png");
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), "image/jpeg");
        assert_eq!(sniff(b"hello, this is plainly text"), "text/plain");
        assert_eq!(
            sniff(&[0x00, 0x01, 0xFE, 0xFF, 0x02]),
            "application/octet-stream"
        );
    }

    #[test]
    fn sniffs_the_media_types() {
        assert_eq!(
            sniff(b"\x00\x00\x00\x18ftypmp42\x00\x00\x00\x00"),
            "video/mp4"
        );
        assert_eq!(
            sniff(b"\x00\x00\x00\x18ftypavif\x00\x00\x00\x00"),
            "image/avif"
        );
        assert_eq!(
            sniff(b"\x00\x00\x00\x18ftypheic\x00\x00\x00\x00"),
            "image/heic"
        );
        assert_eq!(
            sniff(b"\x00\x00\x00\x18ftypmif1\x00\x00\x00\x00"),
            "image/heic"
        );
        assert_eq!(
            sniff(b"\x00\x00\x00\x14ftypqt  \x00\x00\x00\x00"),
            "video/quicktime"
        );
        assert_eq!(sniff(&[0x1A, 0x45, 0xDF, 0xA3, 0x00, 0x00]), "video/webm");
        assert_eq!(sniff(b"RIFF\x00\x00\x00\x00WEBPVP8 "), "image/webp");
        assert_eq!(sniff(b"RIFF\x00\x00\x00\x00WAVEfmt "), "audio/wav");
        assert_eq!(sniff(b"fLaC\x00\x00\x00\x00"), "audio/flac");
        assert_eq!(sniff(b"OggS\x00\x00\x00\x00"), "audio/ogg");
        assert_eq!(sniff(b"ID3\x03\x00\x00\x00"), "audio/mpeg");
        assert_eq!(sniff(&[0xFF, 0xFB, 0x90, 0x00]), "audio/mpeg");
    }

    #[test]
    fn labels_strip_the_path_and_the_control_characters() {
        assert_eq!(label_of("../../etc/passwd"), "passwd");
        assert_eq!(label_of(r"a\b.txt"), "b.txt");
        assert_eq!(label_of("name\r\nwith\tcontrol.txt"), "namewithcontrol.txt");
    }

    #[test]
    fn disposition_carries_no_quote_or_control_character() {
        let header = disposition_of("a\"b\r\nX: y.txt", false);
        assert!(!header.contains('\r'));
        assert!(!header.contains('\n'));
        assert_eq!(
            header,
            "attachment; filename=\"a_b__X__y.txt\"; filename*=UTF-8''a%22b%0D%0AX%3A%20y.txt"
        );
    }

    #[test]
    fn disposition_of_a_traversal_name_keeps_no_path_meaning() {
        let header = disposition_of("../../etc/passwd", false);
        assert_eq!(
            header,
            "attachment; filename=\".._.._etc_passwd\"; filename*=UTF-8''..%2F..%2Fetc%2Fpasswd"
        );
    }

    #[test]
    fn disposition_of_a_non_ascii_name_falls_back_and_percent_encodes() {
        let header = disposition_of("résumé.pdf", false);
        assert_eq!(
            header,
            "attachment; filename=\"r_sum_.pdf\"; filename*=UTF-8''r%C3%A9sum%C3%A9.pdf"
        );
    }

    #[test]
    fn disposition_is_inline_only_when_asked_for() {
        let header = disposition_of("photo.png", true);
        assert!(header.starts_with("inline;"), "{header}");
        let header = disposition_of("photo.png", false);
        assert!(header.starts_with("attachment;"), "{header}");
    }

    #[test]
    fn renders_inline_covers_the_browser_renderable_types_and_nothing_else() {
        assert!(renders_inline("image/png"));
        assert!(renders_inline("video/mp4"));
        assert!(renders_inline("audio/mpeg"));
        assert!(renders_inline("application/pdf"));
        assert!(renders_inline("text/plain"));
        assert!(!renders_inline("application/octet-stream"));
        assert!(!renders_inline("application/zip"));
        assert!(!renders_inline("image/heic"), "no browser decodes heic");
        assert!(renders_inline("video/quicktime"));
    }

    #[test]
    fn viewer_kind_has_no_element_for_plain_text() {
        assert_eq!(viewer_kind("image/png"), Some(ViewerKind::Image));
        assert_eq!(viewer_kind("video/mp4"), Some(ViewerKind::Video));
        assert_eq!(viewer_kind("audio/mpeg"), Some(ViewerKind::Audio));
        assert_eq!(viewer_kind("application/pdf"), Some(ViewerKind::Pdf));
        assert_eq!(viewer_kind("text/plain"), None);
        assert_eq!(viewer_kind("application/octet-stream"), None);
        assert_eq!(
            viewer_kind("image/heic"),
            None,
            "no decoder, no viewer element"
        );
        assert_eq!(viewer_kind("video/quicktime"), Some(ViewerKind::Video));
    }

    #[test]
    fn range_parses_start_end_suffix_and_open_ended() {
        assert_eq!(parse_range("bytes=0-3", 10), Some(Ok((0, 3))));
        assert_eq!(parse_range("bytes=-2", 10), Some(Ok((8, 9))));
        assert_eq!(parse_range("bytes=5-", 10), Some(Ok((5, 9))));
        assert_eq!(
            parse_range("bytes=5-100", 10),
            Some(Ok((5, 9))),
            "end clamps to the last byte"
        );
    }

    #[test]
    fn range_is_unsatisfiable_past_the_end_and_ignored_when_multi_range_or_malformed() {
        assert_eq!(parse_range("bytes=10-20", 10), Some(Err(())));
        assert_eq!(parse_range("bytes=-0", 10), Some(Err(())));
        assert_eq!(
            parse_range("bytes=0-3,5-8", 10),
            None,
            "multi-range falls back to a full body"
        );
        assert_eq!(parse_range("nonsense", 10), None);
        assert_eq!(
            parse_range("bytes=0-3", 0),
            None,
            "an empty file has no range to satisfy"
        );
    }
}
