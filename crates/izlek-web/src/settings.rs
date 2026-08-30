//! Settings, from the Settings and MemberSettings artboards.
//!
//! Which panels a person gets is decided on the server: the sender and the
//! limits belong to an admin, and a Member's answer simply does not carry
//! them — the page cannot hide what it was never sent. Every mutation here
//! checks the role again in its own handler, because a panel that is not
//! drawn is a courtesy and not a guard.
//!
//! Ported from the old UI's `settings.rs`. That version read through a
//! server fn behind a `Resource` and wrote through `ServerAction`s
//! whose return value a hydrated page reads straight off; here the page is
//! rendered server-side on every request and a save is a plain form post, so
//! the answer — success or refusal — travels home the way `files.rs`'s
//! upload already does: appended to the redirect's query, in the same
//! `?refusal=<code>&on=<call>` shape `Refusal::code` was built for.
//!
//! `resend_link` differs from `izlek-web`'s in one respect: there it answered
//! a hydrated `ActionForm` with the mailed address directly. There is no
//! hydration here, so it redirects instead, carrying the address on the
//! query the same way a refusal does.

use topcoat::Result;
use topcoat::context::Cx;
use topcoat::router::content::Form;
use topcoat::router::{HeaderMap, HeaderValue, StatusCode, header, page, route};
use topcoat::view::view;

use izlek_core::detail::ActivityKind;
use izlek_core::store::{NewSender, SenderTest, User};

use crate::i18n::{Key, Lang, t};
use crate::server::{Refusal, accounts, mail, require_admin, require_user};

/// The widest either limit may be set to. A ceiling of any size is a promise
/// the disk has to keep, and a limit typed with one extra zero is a mistake
/// nobody notices until the disk is full.
///
/// `izlek-topcoat::files` carries its own copy until that lane imports this
/// one (per the settings lane's note there).
pub const WIDEST_ATTACHMENT_MB: u64 = 500;
pub const WIDEST_PHOTO_MB: u64 = 20;

const MB: u64 = 1024 * 1024;

/// `1.2 s` for anything over a second, `840 ms` below it. A mail server that
/// answered in under a second is worth saying so precisely; one that took
/// eight is worth a number somebody can compare to the last one.
fn took_label(ms: u64) -> String {
    if ms >= 1000 {
        format!("{:.1} s", ms as f64 / 1000.0)
    } else {
        format!("{ms} ms")
    }
}

/// The shallowest check that catches a typo without rejecting a real
/// address. Nothing here is trying to decide what RFC 5322 permits — the
/// mail server settles that, and a refusal from it lands in the ledger with
/// its own words.
fn is_address(raw: &str) -> bool {
    let mut halves = raw.split('@');
    let (Some(local), Some(domain), None) = (halves.next(), halves.next(), halves.next()) else {
        return false;
    };
    !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !raw.contains(char::is_whitespace)
}

/// The typed list as extensions, or `None` if one of them is not an
/// extension.
///
/// Lowercased, dots dropped, duplicates dropped, and nothing but letters and
/// digits kept — a "type" with a slash or a dot in it would be a path or a
/// pattern wearing an extension's clothes, and this list is checked against a
/// filename later.
fn parse_types(raw: &str) -> Option<Vec<String>> {
    let mut types: Vec<String> = Vec::new();
    for piece in raw.split([',', ' ', '\n']) {
        let piece = piece.trim().trim_start_matches('.').to_lowercase();
        if piece.is_empty() {
            continue;
        }
        if piece.len() > 12 || !piece.chars().all(|c| c.is_ascii_alphanumeric()) {
            return None;
        }
        if !types.contains(&piece) {
            types.push(piece);
        }
    }
    Some(types)
}

/// A query value survives the round trip to a `Location` header only if it
/// cannot be mistaken for another pair or break the header outright — a
/// mailed address may hold `+`, `&`, `=`, none of which a hand-built query
/// string may pass through raw.
pub(crate) fn encode_q(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

pub(crate) fn decode_q(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(value) = u8::from_str_radix(&raw[i + 1..i + 3], 16) {
                out.push(value);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod codec_tests {
    use super::{decode_q, encode_q, parse_types, took_label};

    #[test]
    fn an_address_survives_the_round_trip() {
        for address in ["a@b.com", "a+tag@b.com", "a&b=c@b.com", "weird%chars@b.com"] {
            assert_eq!(decode_q(&encode_q(address)), address);
        }
    }

    #[test]
    fn a_list_is_lowercased_undotted_and_deduplicated() {
        assert_eq!(
            parse_types(".PNG, png, jpg").unwrap(),
            vec!["png".to_string(), "jpg".to_string()]
        );
    }

    #[test]
    fn an_empty_list_means_every_type() {
        assert_eq!(parse_types("  ").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn nothing_that_is_not_an_extension_is_stored_as_one() {
        assert!(parse_types("image/png").is_none());
        assert!(parse_types("../etc").is_none());
        assert!(parse_types("*").is_none());
        assert!(parse_types("tar.gz").is_none());
    }

    #[test]
    fn a_send_is_timed_the_way_the_artboard_writes_it() {
        assert_eq!(took_label(1234), "1.2 s");
        assert_eq!(took_label(1000), "1.0 s");
        assert_eq!(took_label(999), "999 ms");
        assert_eq!(took_label(0), "0 ms");
    }
}

/// A 303 to `/settings`, with `query` appended.
fn redirect_to(query: &str) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut headers = HeaderMap::new();
    let location = if query.is_empty() {
        "/settings".to_string()
    } else {
        format!("/settings?{query}")
    };
    if let Ok(value) = HeaderValue::from_str(&location) {
        headers.insert(header::LOCATION, value);
    }
    (StatusCode::SEE_OTHER, headers, Vec::new())
}

/// Which rail section a call's redirect lands back on, so the reload shows
/// the panel the form it answers actually lives in.
fn section_of_call(call: &str) -> &'static str {
    match call {
        "save_sender" | "send_test_mail" => "outgoing",
        "save_limits" => "limits",
        "invite_member" | "set_role" | "resend_link" => "members",
        "send_message" => "message",
        _ => "profile",
    }
}

/// Where a save lands: back on Settings, with the refusal (if any) on the
/// query the same way `files.rs`'s upload carries one, or `saved=<call>`
/// when there was nothing to refuse — either way, on the rail section that
/// call belongs to.
pub(crate) fn saved_or_refused(
    call: &str,
    refusal: Option<Refusal>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let section = section_of_call(call);
    match refusal {
        // `BadSender`'s code collapses six different complaints into one
        // generic "bad-sender"; the sentence save time actually wrote rides
        // along as `why` so the redirect does not have to guess it back.
        Some(Refusal::BadSender(problem)) => redirect_to(&format!(
            "refusal=bad-sender&on={call}&why={}&section={section}",
            qsencode(&problem)
        )),
        Some(refusal) => redirect_to(&format!(
            "refusal={}&on={call}&section={section}",
            refusal.code()
        )),
        None => redirect_to(&format!("saved={call}&section={section}")),
    }
}

/// Percent-encodes just enough that a sentence with spaces and punctuation
/// survives round-tripping through a `&`/`=`-split query string. The
/// messages this carries are static and ASCII, so this stays byte-wise
/// rather than reaching for a full percent-encoding crate.
fn qsencode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            b' ' => "+".to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The inverse of `qsencode`. Works on bytes, not `char`s, so a multi-byte
/// UTF-8 sequence that `qsencode` split into separate `%XX` escapes comes
/// back together rather than one mangled `char` per byte.
fn qsdecode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => match u8::from_str_radix(&text[i + 1..i + 3], 16) {
                Ok(byte) => {
                    out.push(byte);
                    i += 3;
                }
                Err(_) => {
                    out.push(b'%');
                    i += 1;
                }
            },
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct SaveLimitsForm {
    attachment_limit_mb: u64,
    photo_limit_mb: u64,
    allowed_file_types: String,
}

/// Changes the workspace's limits. Admin-only, checked here.
///
/// The list is parsed rather than trusted: it is what a later upload is
/// checked against, so a type that is not a plain extension has no business
/// being stored as one.
#[route(POST "/api/save_limits")]
async fn save_limits(
    cx: &Cx,
    Form(input): Form<SaveLimitsForm>,
) -> Result<(StatusCode, HeaderMap, Vec<u8>)> {
    let admin = match require_admin(cx).await {
        Ok(admin) => admin,
        Err(refusal) => return Ok(saved_or_refused("save_limits", Some(refusal))),
    };
    if input.attachment_limit_mb == 0
        || input.photo_limit_mb == 0
        || input.attachment_limit_mb > WIDEST_ATTACHMENT_MB
        || input.photo_limit_mb > WIDEST_PHOTO_MB
    {
        return Ok(saved_or_refused("save_limits", Some(Refusal::BadLimit)));
    }
    let Some(types) = parse_types(&input.allowed_file_types) else {
        return Ok(saved_or_refused("save_limits", Some(Refusal::BadFileType)));
    };
    let outcome = accounts(cx)
        .store()
        .set_limits(
            &admin.workspace_id,
            input.attachment_limit_mb * MB,
            input.photo_limit_mb * MB,
            &types,
        )
        .await;
    let refusal = match outcome {
        Ok(()) => {
            let _ = accounts(cx)
                .store()
                .record_event(
                    Some(&admin.id),
                    &ActivityKind::LimitsSaved,
                    "",
                    time::OffsetDateTime::now_utc(),
                )
                .await;
            None
        }
        Err(problem) => {
            eprintln!("store error: {problem}");
            Some(Refusal::Unavailable)
        }
    };
    Ok(saved_or_refused("save_limits", refusal))
}

#[derive(serde::Deserialize)]
struct SaveSenderForm {
    host: String,
    port: u32,
    username: String,
    #[serde(default)]
    password: String,
    from_name: String,
    from_address: String,
    /// The origin mailed links point at. Empty falls back to the address the
    /// process was configured with.
    #[serde(default)]
    public_url: String,
}

/// Writes the workspace's sender. Admin-only, checked here.
///
/// `password` is empty when the admin did not type one, and empty means
/// "leave the stored one alone" rather than "delete it": the field is
/// write-only, so the form has nothing to send back for a password that
/// already exists, and a save that took the blank literally would stop the
/// workspace sending mail as a side effect of fixing a typo in the port.
///
/// A sender with no password at all is refused rather than stored, because a
/// half-filled sender is not a configuration — it is a mail nobody receives
/// and a ledger nobody reads.
#[route(POST "/api/save_sender")]
async fn save_sender(
    cx: &Cx,
    Form(input): Form<SaveSenderForm>,
) -> Result<(StatusCode, HeaderMap, Vec<u8>)> {
    let admin = match require_admin(cx).await {
        Ok(admin) => admin,
        Err(refusal) => return Ok(saved_or_refused("save_sender", Some(refusal))),
    };
    let lang = Lang::from_code(&admin.language);

    let host = input.host.trim().to_string();
    let username = input.username.trim().to_string();
    let from_name = input.from_name.trim().to_string();
    let from_address = input.from_address.trim().to_string();
    // Trailing slashes go: every link built from this appends its own path,
    // and `https://izlek.sh//?task=X` is a link that works by luck.
    let public_url = input.public_url.trim().trim_end_matches('/').to_string();
    // Not typed is not the same as blanked. Nothing here can clear a stored
    // password; replacing it means typing a new one.
    let password = (!input.password.is_empty()).then_some(input.password);

    let store = accounts(cx).store().clone();
    let known = store
        .workspace()
        .await?
        .map(|ws| ws.smtp_password_set)
        .unwrap_or(false);

    let complaint = if host.is_empty() {
        Some(Key::SmtpHostRequired)
    } else if host.contains(char::is_whitespace) || host.contains('@') || host.contains('/') {
        Some(Key::SmtpHostInvalid)
    } else if !(1..=65535).contains(&input.port) {
        Some(Key::PortInvalid)
    } else if username.is_empty() {
        Some(Key::SmtpUsernameRequired)
    } else if password.is_none() && !known {
        Some(Key::PasswordNeededFirstTime)
    } else if !is_address(&from_address) {
        Some(Key::NotFromAddress)
    } else if !public_url.is_empty() && !is_origin(&public_url) {
        Some(Key::NotAnOrigin)
    } else {
        None
    };
    if let Some(problem) = complaint {
        return Ok(saved_or_refused(
            "save_sender",
            Some(Refusal::BadSender(t(lang, problem).to_string())),
        ));
    }

    let outcome = match store
        .set_sender(
            &admin.workspace_id,
            NewSender {
                host,
                port: input.port,
                username,
                password,
                from_name,
                from_address,
            },
        )
        .await
    {
        Ok(()) => {
            store
                .set_public_url(
                    &admin.workspace_id,
                    (!public_url.is_empty()).then_some(public_url.as_str()),
                )
                .await
        }
        Err(problem) => Err(problem),
    };
    let refusal = match outcome {
        Ok(()) => {
            let _ = store
                .record_event(
                    Some(&admin.id),
                    &ActivityKind::SenderSaved,
                    "",
                    time::OffsetDateTime::now_utc(),
                )
                .await;
            None
        }
        Err(problem) => {
            eprintln!("store error: {problem}");
            Some(Refusal::Unavailable)
        }
    };
    Ok(saved_or_refused("save_sender", refusal))
}

/// Sends one mail to the admin who pressed the button, and writes down how it
/// went so the answer survives a reload.
///
/// It goes to their own address and nowhere else. A test that could be
/// pointed at an address somebody typed would be a way to make Izlek mail a
/// stranger on demand, which is a thing worth not building.
#[route(POST "/api/send_test_mail")]
async fn send_test_mail(cx: &Cx) -> Result<(StatusCode, HeaderMap, Vec<u8>)> {
    let admin = match require_admin(cx).await {
        Ok(admin) => admin,
        Err(refusal) => return Ok(saved_or_refused("send_test_mail", Some(refusal))),
    };
    let lang = Lang::from_code(&admin.language);

    let store = accounts(cx).store().clone();
    let configured = store
        .workspace()
        .await?
        .is_some_and(|ws| ws.smtp_password_set && ws.smtp_host.is_some());
    if !configured {
        return Ok(saved_or_refused(
            "send_test_mail",
            Some(Refusal::BadSender(
                t(lang, Key::SenderNotConfiguredYet).to_string(),
            )),
        ));
    }

    let Some(outcome) = mail(cx).test(&admin.email).await else {
        // No engine in this process at all. Nothing sends here, and saying so
        // beats writing down a failure the settings did not cause.
        return Ok(saved_or_refused(
            "send_test_mail",
            Some(Refusal::Unavailable),
        ));
    };

    let at = time::OffsetDateTime::now_utc();
    let test = match outcome {
        Ok(took) => SenderTest {
            at,
            took_ms: took.whole_milliseconds().max(0) as u64,
            error: None,
        },
        Err(problem) => SenderTest {
            at,
            took_ms: 0,
            // The mailer builds this from what the server said, never from
            // the credentials it sent, so it is safe to store and to show.
            error: Some(problem.message.clone()),
        },
    };
    let sent = test.error.is_none();
    if let Err(problem) = store.record_sender_test(&admin.workspace_id, test).await {
        eprintln!("store error: {problem}");
        return Ok(saved_or_refused(
            "send_test_mail",
            Some(Refusal::Unavailable),
        ));
    }
    if sent {
        let _ = store
            .record_event(
                Some(&admin.id),
                &ActivityKind::TestMailSent,
                "",
                time::OffsetDateTime::now_utc(),
            )
            .await;
    }
    Ok(saved_or_refused("send_test_mail", None))
}

#[derive(serde::Deserialize)]
struct SaveProfileForm {
    display_name: String,
    /// Absent on a form that never got the field — kept as it was, same as
    /// `timezone`/`theme`/`language`/`ui` below.
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    timezone: Option<String>,
    #[serde(default)]
    theme: Option<String>,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    ui: Option<String>,
}

/// The values the theme field offers.
const THEME_OPTIONS: [&str; 2] = ["light", "dark"];

/// The values the ui field offers.
const UI_OPTIONS: [&str; 2] = ["instrument", "ledger"];

/// The values the language field offers.
const LANGUAGE_OPTIONS: [&str; 2] = ["en", "tr"];

/// The offsets the timezone field offers, `"UTC-12:00"` through `"UTC+14:00"`.
///
/// Decision: `time` (already a dependency) has no tz-database, and this
/// workspace has no other crate that carries one — adding one just for a
/// display label is a new dependency for what "show logs in my timezone"
/// does not need. Fixed offsets satisfy it; `"UTC"` stands for +00:00.
fn zone_options() -> Vec<String> {
    (-12..=14)
        .map(|hour: i32| {
            if hour == 0 {
                "UTC".to_string()
            } else {
                format!(
                    "UTC{}{:02}:00",
                    if hour > 0 { "+" } else { "-" },
                    hour.abs()
                )
            }
        })
        .collect()
}

/// Renames the person asking and sets their display-only preferences. Nobody
/// touches anybody else here: the id comes from the session, never from the
/// form.
#[route(POST "/api/save_profile")]
async fn save_profile(
    cx: &Cx,
    Form(input): Form<SaveProfileForm>,
) -> Result<(StatusCode, HeaderMap, Vec<u8>)> {
    let user = match require_user(cx).await {
        Ok(user) => user,
        Err(refusal) => return Ok(saved_or_refused("save_profile", Some(refusal))),
    };
    let display_name = input.display_name.trim().to_string();
    if display_name.is_empty() {
        return Ok(saved_or_refused("save_profile", Some(Refusal::EmptyName)));
    }
    let timezone = input.timezone.unwrap_or_else(|| user.timezone.clone());
    if !zone_options().contains(&timezone) {
        return Ok(saved_or_refused("save_profile", Some(Refusal::BadZone)));
    }
    let theme = input.theme.unwrap_or_else(|| user.theme.clone());
    if !THEME_OPTIONS.contains(&theme.as_str()) {
        return Ok(saved_or_refused("save_profile", Some(Refusal::BadTheme)));
    }
    let ui = input.ui.unwrap_or_else(|| user.ui.clone());
    if !UI_OPTIONS.contains(&ui.as_str()) {
        return Ok(saved_or_refused("save_profile", Some(Refusal::BadUi)));
    }
    let language = input.language.unwrap_or_else(|| user.language.clone());
    if !LANGUAGE_OPTIONS.contains(&language.as_str()) {
        return Ok(saved_or_refused("save_profile", Some(Refusal::BadLanguage)));
    }
    let email = input
        .email
        .map(|e| e.trim().to_lowercase())
        .unwrap_or_else(|| user.email.clone());
    if !is_address(&email) {
        return Ok(saved_or_refused("save_profile", Some(Refusal::BadEmail)));
    }
    let accounts = accounts(cx);
    let store = accounts.store();
    // Checked before anything is written: a taken email must refuse the whole
    // save, not leave the name/theme/language/timezone changes standing while
    // only the email is turned away.
    if email != user.email
        && store
            .user_by_email(&user.workspace_id, &email)
            .await?
            .is_some()
    {
        return Ok(saved_or_refused(
            "save_profile",
            Some(Refusal::AddressTaken),
        ));
    }
    let outcome = store
        .set_profile(&user.id, &display_name)
        .await
        .and(
            store
                .set_preferences(&user.id, &timezone, &theme, &language, &ui)
                .await,
        )
        .and(store.set_email(&user.id, &user.workspace_id, &email).await);
    let refusal = match outcome {
        Ok(()) => {
            let _ = store
                .record_event(
                    Some(&user.id),
                    &ActivityKind::ProfileSaved,
                    "",
                    time::OffsetDateTime::now_utc(),
                )
                .await;
            None
        }
        Err(izlek_core::store::StoreError::Conflict("account")) => Some(Refusal::AddressTaken),
        Err(problem) => {
            eprintln!("store error: {problem}");
            Some(Refusal::Unavailable)
        }
    };
    Ok(saved_or_refused("save_profile", refusal))
}

#[derive(serde::Deserialize)]
struct ResendLinkForm {
    user_id: String,
}

/// Sends the same person another first-sign-in link by mail. Admin-only,
/// checked here.
///
/// There is no hydrated action here to read a return value off of, so the
/// mailed address travels home on the redirect's query, the same way a
/// refusal does.
#[route(POST "/api/resend_link")]
async fn resend_link(
    cx: &Cx,
    Form(input): Form<ResendLinkForm>,
) -> Result<(StatusCode, HeaderMap, Vec<u8>)> {
    let admin = match require_admin(cx).await {
        Ok(admin) => admin,
        Err(refusal) => return Ok(saved_or_refused("resend_link", Some(refusal))),
    };
    match accounts(cx).resend_invitation(&admin, &input.user_id).await {
        Ok(invitation) => {
            mail(cx).after_invite();
            let _ = accounts(cx)
                .store()
                .record_event(
                    Some(&admin.id),
                    &ActivityKind::LinkResent,
                    &invitation.user.email,
                    time::OffsetDateTime::now_utc(),
                )
                .await;
            Ok(redirect_to(&format!(
                "mailed={}",
                encode_q(&invitation.user.email)
            )))
        }
        Err(error) => Ok(saved_or_refused("resend_link", Some(error.into()))),
    }
}

#[derive(serde::Deserialize)]
struct SetRoleForm {
    user_id: String,
    role: izlek_core::Role,
}

/// Changes a member's role. Admin-only; the owner's row and the caller's own
/// row are refused, checked in `Accounts::set_role`.
#[route(POST "/api/set_role")]
async fn set_role(
    cx: &Cx,
    Form(input): Form<SetRoleForm>,
) -> Result<(StatusCode, HeaderMap, Vec<u8>)> {
    let admin = match require_admin(cx).await {
        Ok(admin) => admin,
        Err(refusal) => return Ok(saved_or_refused("set_role", Some(refusal))),
    };
    let store = accounts(cx).store().clone();
    match accounts(cx)
        .set_role(&admin, &input.user_id, input.role)
        .await
    {
        Ok(()) => {
            let name = store
                .user(&input.user_id)
                .await?
                .map(|member| member.display_name)
                .unwrap_or_default();
            let _ = store
                .record_event(
                    Some(&admin.id),
                    &ActivityKind::RoleChanged,
                    &format!("{name} -> {}", input.role.as_str()),
                    time::OffsetDateTime::now_utc(),
                )
                .await;
            Ok(saved_or_refused("set_role", None))
        }
        Err(error) => Ok(saved_or_refused("set_role", Some(error.into()))),
    }
}

#[derive(serde::Deserialize)]
struct SendMessageForm {
    to: String,
    subject: String,
    body: String,
}

#[route(POST "/api/send_message")]
async fn send_message(
    cx: &Cx,
    Form(input): Form<SendMessageForm>,
) -> Result<(StatusCode, HeaderMap, Vec<u8>)> {
    let admin = match require_admin(cx).await {
        Ok(admin) => admin,
        Err(refusal) => return Ok(saved_or_refused("send_message", Some(refusal))),
    };
    let subject = input.subject.trim();
    if subject.is_empty() {
        return Ok(saved_or_refused("send_message", Some(Refusal::EmptySubject)));
    }
    let body = input.body.trim();
    if body.is_empty() {
        return Ok(saved_or_refused("send_message", Some(Refusal::EmptyBody)));
    }
    let store = accounts(cx).store().clone();
    let members = store.users(&admin.workspace_id).await?;
    let recipients: Vec<String> = if input.to == "everyone" {
        members
            .into_iter()
            .filter(|member| member.id != admin.id)
            .map(|member| member.email)
            .collect()
    } else {
        match members.into_iter().find(|member| member.id == input.to) {
            Some(member) => vec![member.email],
            None => return Ok(saved_or_refused("send_message", Some(Refusal::NoSuchMember))),
        }
    };
    let now = time::OffsetDateTime::now_utc();
    for recipient in &recipients {
        if let Err(problem) = store.queue_notice(recipient, subject, body, now).await {
            eprintln!("store error: {problem}");
            return Ok(saved_or_refused("send_message", Some(Refusal::Unavailable)));
        }
    }
    let _ = store
        .record_event(Some(&admin.id), &ActivityKind::MessageSent, subject, now)
        .await;
    Ok(saved_or_refused("send_message", None))
}

// ---------------------------------------------------------------------------
// Page
// ---------------------------------------------------------------------------

/// One row of the member list, as an admin may see it. There is no password
/// and no token here: whether a password exists is a fact about the account,
/// and the token of a live link is shown once at the moment it is minted and
/// is never readable afterwards — the store keeps only its hash.
struct Member {
    id: String,
    display_name: String,
    has_photo: bool,
    email: String,
    role: izlek_core::Role,
    has_password: bool,
    /// The day they last signed in, as the list writes it, or nothing.
    last_signed_in: Option<String>,
    is_you: bool,
    /// The first account. It administers the workspace and cannot be
    /// removed.
    is_owner: bool,
}

async fn members_now(cx: &Cx, asking: &User) -> Result<Vec<Member>> {
    let store = accounts(cx).store().clone();
    let owner = store.owner().await?.map(|owner| owner.id);
    let users = store.users(&asking.workspace_id).await?;
    let zone = izlek_core::detail::parse_zone(&asking.timezone);
    Ok(users
        .into_iter()
        .map(|user| Member {
            has_password: user.password_hash.is_some(),
            last_signed_in: user
                .last_signed_in_at
                .map(|at| izlek_core::board::day_label(at.to_offset(zone).date())),
            is_you: user.id == asking.id,
            is_owner: owner.as_deref() == Some(user.id.as_str()),
            id: user.id,
            display_name: user.display_name,
            has_photo: user.has_photo,
            email: user.email,
            role: user.role,
        })
        .collect())
}

async fn limits_now(cx: &Cx, workspace_id: &str) -> Result<(u64, u64, Vec<String>)> {
    let workspace = accounts(cx)
        .store()
        .workspace()
        .await?
        .filter(|workspace| workspace.id == workspace_id)
        .ok_or_else(|| topcoat::Error::from(std::io::Error::other("no workspace")))?;
    Ok((
        workspace.attachment_limit_bytes / MB,
        workspace.photo_limit_bytes / MB,
        workspace.allowed_file_types,
    ))
}

/// What the row under the test button says, once the workspace holds a
/// `sender_test` to read.
struct TestResult {
    moment: String,
    took: Option<String>,
    error: Option<String>,
}

/// The sender as the admin's screen may see it, plus the last test's
/// outcome — persisted, so it survives the reload the redirect gives it
/// rather than living only in this request's memory.
struct Sender {
    host: String,
    port: u16,
    username: String,
    from_name: String,
    from_address: String,
    password_set: bool,
    test: Option<TestResult>,
    /// The origin mailed links point at, empty when the configured one is
    /// still in use.
    public_url: String,
}

impl Sender {
    fn is_connected(&self) -> bool {
        !self.host.trim().is_empty()
            && !self.username.trim().is_empty()
            && !self.from_address.trim().is_empty()
            && self.password_set
    }
}

async fn sender_now(cx: &Cx, zone: time::UtcOffset) -> Result<Sender> {
    let workspace = accounts(cx).store().workspace().await?;
    Ok(match workspace {
        None => Sender {
            host: String::new(),
            port: 587,
            username: String::new(),
            from_name: String::new(),
            from_address: String::new(),
            password_set: false,
            test: None,
            public_url: String::new(),
        },
        Some(workspace) => Sender {
            host: workspace.smtp_host.unwrap_or_default(),
            port: workspace
                .smtp_port
                .and_then(|p| u16::try_from(p).ok())
                .unwrap_or(587),
            username: workspace.smtp_username.unwrap_or_default(),
            from_name: workspace.smtp_from_name.unwrap_or_default(),
            from_address: workspace.smtp_from_address.unwrap_or_default(),
            password_set: workspace.smtp_password_set,
            test: workspace.sender_test.map(|test| TestResult {
                moment: izlek_core::detail::moment_label_in(test.at, zone),
                took: test.error.is_none().then(|| took_label(test.took_ms)),
                error: test.error,
            }),
            public_url: workspace.public_url.unwrap_or_default(),
        },
    })
}

/// Whether a typed address is an origin a link can be built on: an http or
/// https scheme, a host after it, and no whitespace anywhere. A path is
/// allowed — Izlek behind `example.com/izlek` is a real deployment — but a
/// bare host or a mail address is not.
fn is_origin(value: &str) -> bool {
    let Some(rest) = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
    else {
        return false;
    };
    let host = rest.split('/').next().unwrap_or_default();
    !host.is_empty()
        && !host.contains('@')
        && !value.chars().any(char::is_whitespace)
}

/// The refusal that landed on this call's redirect, if any, and this call's
/// own `saved=<call>` flag.
fn call_state<'q>(query: &'q str, call: &str) -> (Option<Refusal>, bool) {
    let mut refusal_code = None;
    let mut on = None;
    let mut saved = false;
    let mut why = None;
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        match key {
            "refusal" => refusal_code = Some(value),
            "on" => on = Some(value),
            "why" => why = Some(value),
            "saved" if value == call => saved = true,
            _ => {}
        }
    }
    let refusal = if on == Some(call) {
        refusal_code
            .and_then(Refusal::from_code)
            .map(|refusal| match (refusal, why) {
                // The generic "bad-sender" fallback is what a tampered or absent
                // `why` gets; a real one carries save time's own sentence.
                (Refusal::BadSender(_), Some(why)) => Refusal::BadSender(qsdecode(why)),
                (refusal, _) => refusal,
            })
    } else {
        None
    };
    (refusal, saved)
}

fn query_value<'q>(query: &'q str, key: &str) -> Option<&'q str> {
    query.split('&').find_map(|pair| {
        pair.split_once('=')
            .filter(|(k, _)| *k == key)
            .map(|(_, v)| v)
    })
}

/// Which rail section the page renders. Only one is drawn at a time; an
/// admin-only value asked for by anyone else falls back to `Profile`, same as
/// a section name it does not recognize at all.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Profile,
    Outgoing,
    Limits,
    Members,
    Message,
}

/// The class a rail link wears: `active` on the section it points to when
/// that is the one showing, plain otherwise.
fn rail_class(current: Section, target: Section) -> &'static str {
    if current == target {
        "settings-section-link active"
    } else {
        "settings-section-link"
    }
}

/// The settings screen.
#[page("/settings")]
#[allow(unused_variables)]
async fn settings_page(cx: &Cx) -> Result {
    let user = match require_user(cx).await {
        Ok(user) => user,
        Err(refusal) => {
            return view! {
                cx =>
                <main class="scaffold-note">
                    <p>(refusal.message())</p>
                    <p><a href="/">(t(Lang::En, Key::BackToBoard))</a></p>
                </main>
            };
        }
    };
    let lang = Lang::from_code(&user.language);
    let administers = user.role.can_administer();
    let query = topcoat::router::request::uri(cx).query().unwrap_or("");
    let section = match query_value(query, "section") {
        Some("outgoing") if administers => Section::Outgoing,
        Some("limits") if administers => Section::Limits,
        Some("members") if administers => Section::Members,
        Some("message") if administers => Section::Message,
        _ => Section::Profile,
    };

    let zone = izlek_core::detail::parse_zone(&user.timezone);
    // What a link falls back to when the field is empty — the placeholder
    // shows it rather than an invented example.
    let configured_url = accounts(cx).configured_url().to_string();
    let sender = if administers {
        Some(sender_now(cx, zone).await?)
    } else {
        None
    };
    let (limits, allowed_types) = if administers {
        let (attachment, photo, types) = limits_now(cx, &user.workspace_id).await?;
        (Some((attachment, photo)), types)
    } else {
        (None, Vec::new())
    };
    let members = if administers {
        Some(members_now(cx, &user).await?)
    } else {
        None
    };

    let (profile_refusal, profile_saved) = call_state(query, "save_profile");
    let (photo_refusal, photo_saved) = call_state(query, "profile_photo");
    let (photo_delete_refusal, _) = call_state(query, "delete_profile_photo");
    let photo_refusal = photo_refusal.or(photo_delete_refusal);
    let (sender_refusal, sender_saved) = call_state(query, "save_sender");
    let (test_refusal, _) = call_state(query, "send_test_mail");
    let (limits_refusal, limits_saved) = call_state(query, "save_limits");
    let (resend_refusal, _) = call_state(query, "resend_link");
    let (invite_refusal, _) = call_state(query, "invite_member");
    let (role_refusal, _) = call_state(query, "set_role");
    let member_refusal = resend_refusal.or(invite_refusal).or(role_refusal);
    let (password_refusal, _) = call_state(query, "change_password");
    let (message_refusal, message_saved) = call_state(query, "send_message");
    let mailed = query_value(query, "mailed").map(decode_q);

    view! {
        cx =>
        <header class="topbar">
            <a class="wordmark" href="/">
                <span class="wordmark-text">"izlek"</span>
                <span class="wordmark-dot"></span>
            </a>
            (crate::layout::topbar_nav(cx, crate::layout::NavPage::Settings, user.role, lang).await?)
            <div class="spacer"></div>
            (crate::layout::user_menu(cx, &crate::detail::Me::from(&user), lang).await?)
        </header>

        <div class="settings-shell">
            <nav class="settings-sections">
                <a class=(rail_class(section, Section::Profile)) href="/settings?section=profile">(t(lang, Key::YourProfile))</a>
                if administers {
                    <a class=(rail_class(section, Section::Outgoing)) href="/settings?section=outgoing">(t(lang, Key::OutgoingMail))</a>
                    <a class=(rail_class(section, Section::Limits)) href="/settings?section=limits">(t(lang, Key::WorkspaceLimits))</a>
                    <a class=(rail_class(section, Section::Members)) href="/settings?section=members">(t(lang, Key::Members))</a>
                    <a class=(rail_class(section, Section::Message)) href="/settings?section=message">(t(lang, Key::Message))</a>
                }
            </nav>
            <main class="settings-stage">
                if section == Section::Profile {
                <section class="panel" id="profile">
                    <div class="panel-head">
                        <h2 class="panel-title">(t(lang, Key::YourProfile))</h2>
                    </div>
                    <form method="post" action="/api/save_profile" class="panel-body">
                        <div class="identity-row">
                            <label class="avatar-upload">
                                (crate::layout::avatar(cx, &izlek_core::board::Person {
                                    id: user.id.clone(),
                                    display_name: user.display_name.clone(),
                                    has_photo: user.has_photo,
                                }, "avatar-lg").await?)
                                <input class="file-upload-input" type="file" name="file" accept="image/*" data-autosubmit="" form="photo-form">
                            </label>
                            if user.has_photo {
                                <button class="quiet" type="submit" form="photo-remove-form">(t(lang, Key::Remove))</button>
                            }
                            if let Some(refusal) = &photo_refusal {
                                <span class="field-error">(refusal.message_in(lang))</span>
                            }
                            if photo_saved {
                                <span class="field-note">(t(lang, Key::Saved))</span>
                            }
                        </div>
                        <label class="field">
                            <span class="field-label">(t(lang, Key::DisplayNameLabel))</span>
                            <input
                                class="field-input"
                                type="text"
                                name="display_name"
                                value=(user.display_name.clone())
                                maxlength="80"
                                required=""
                            >
                        </label>
                        <label class="field">
                            <span class="field-label">(t(lang, Key::EmailLabel))</span>
                            <input class="field-input" type="email" name="email" value=(user.email.clone()) required="">
                        </label>
                        <label class="field">
                            <span class="field-label">(t(lang, Key::TimezoneLabel))</span>
                            <select class="field-input" name="timezone">
                                for zone in zone_options() {
                                    <option value=(zone.clone()) selected=(zone == user.timezone)>(zone)</option>
                                }
                            </select>
                        </label>
                        <label class="field">
                            <span class="field-label">(t(lang, Key::ThemeLabel))</span>
                            <select class="field-input" name="theme">
                                <option value="light" selected=(user.theme == "light")>(t(lang, Key::LightOption))</option>
                                <option value="dark" selected=(user.theme == "dark")>(t(lang, Key::DarkOption))</option>
                            </select>
                        </label>
                        <label class="field">
                            <span class="field-label">(t(lang, Key::UiLabel))</span>
                            <select class="field-input" name="ui">
                                <option value="instrument" selected=(user.ui == "instrument")>"Instrument"</option>
                                <option value="ledger" selected=(user.ui == "ledger")>"Ledger"</option>
                            </select>
                        </label>
                        <label class="field">
                            <span class="field-label">(t(lang, Key::LanguageLabel))</span>
                            <select class="field-input" name="language">
                                <option value="en" selected=(user.language == "en")>"English"</option>
                                <option value="tr" selected=(user.language == "tr")>"Türkçe"</option>
                            </select>
                        </label>
                        <div class="panel-foot">
                            if let Some(refusal) = &profile_refusal {
                                <span class="field-error">(refusal.message_in(lang))</span>
                            }
                            if profile_saved {
                                <span class="field-note">(t(lang, Key::Saved))</span>
                            }
                            <button class="primary" type="submit">(t(lang, Key::Save))</button>
                        </div>
                    </form>
                    <form id="photo-form" method="post" action="/api/profile_photo" enctype="multipart/form-data"></form>
                    <form id="photo-remove-form" method="post" action="/api/delete_profile_photo"></form>
                </section>

                <section class="panel" id="password">
                    <div class="panel-head">
                        <h2 class="panel-title">(t(lang, Key::ChangePassword))</h2>
                    </div>
                    <form method="post" action="/api/change_password" class="panel-body">
                        <label class="field">
                            <span class="field-label">(t(lang, Key::CurrentPasswordLabel))</span>
                            <input class="field-input" type="password" name="current" autocomplete="current-password" required="">
                        </label>
                        <label class="field">
                            <span class="field-label">(t(lang, Key::NewPasswordLabel))</span>
                            <input class="field-input" type="password" name="new" autocomplete="new-password" required="">
                        </label>
                        <div class="panel-foot">
                            if let Some(refusal) = &password_refusal {
                                <span class="field-error">(refusal.message_in(lang))</span>
                            }
                            <button class="primary" type="submit">(t(lang, Key::ChangePassword))</button>
                        </div>
                    </form>
                </section>
                }

                if section == Section::Outgoing && let Some(sender) = &sender {
                    let connected = sender.is_connected();
                    let password_set = sender.password_set;
                    <section class="panel" id="outgoing">
                        <div class="panel-head">
                            <h2 class="panel-title">(t(lang, Key::OutgoingMail))</h2>
                            <span class="chip chip-admin">(t(lang, Key::AdminOnly))</span>
                            if connected {
                                <span class="chip chip-connected">(t(lang, Key::Connected))</span>
                            } else {
                                <span class="chip chip-off">(t(lang, Key::NotConfiguredChip))</span>
                            }
                        </div>
                        <div class="panel-body">
                            if !connected {
                                <p class="panel-lede">(t(lang, Key::NotConnectedNote))</p>
                            }
                            <form method="post" action="/api/save_sender" id="sender-settings">
                                <div class="field-row">
                                    <label class="field">
                                        <span class="field-label">(t(lang, Key::SmtpHostLabel))</span>
                                        <input
                                            class="field-input"
                                            type="text"
                                            name="host"
                                            value=(sender.host.clone())
                                            placeholder="smtp.fastmail.com"
                                        >
                                    </label>
                                    <label class="field field-narrow">
                                        <span class="field-label">(t(lang, Key::PortLabel))</span>
                                        <input
                                            class="field-input"
                                            type="number"
                                            name="port"
                                            min="1"
                                            max="65535"
                                            value=(sender.port.to_string())
                                        >
                                    </label>
                                </div>
                                <label class="field">
                                    <span class="field-label">(t(lang, Key::UsernameLabel))</span>
                                    <input class="field-input" type="text" name="username" value=(sender.username.clone())>
                                </label>
                                <label class="field">
                                    <span class="field-label">(t(lang, Key::PasswordLabel))</span>
                                    <input
                                        class="field-input"
                                        type="password"
                                        name="password"
                                        autocomplete="off"
                                        placeholder=(if password_set { t(lang, Key::PasswordSetPlaceholder) } else { t(lang, Key::PasswordNeededPlaceholder) })
                                    >
                                </label>
                                <div class="field-row">
                                    <label class="field">
                                        <span class="field-label">(t(lang, Key::FromNameLabel))</span>
                                        <input
                                            class="field-input"
                                            type="text"
                                            name="from_name"
                                            value=(sender.from_name.clone())
                                            placeholder="Izlek"
                                        >
                                    </label>
                                    <label class="field">
                                        <span class="field-label">(t(lang, Key::FromAddressLabel))</span>
                                        <input
                                            class="field-input"
                                            type="text"
                                            name="from_address"
                                            value=(sender.from_address.clone())
                                            placeholder="board@izlek.sh"
                                        >
                                    </label>
                                </div>
                                <label class="field">
                                    <span class="field-label">(t(lang, Key::LinkAddressLabel))</span>
                                    <input
                                        class="field-input"
                                        type="text"
                                        name="public_url"
                                        value=(sender.public_url.clone())
                                        placeholder=(configured_url.clone())
                                    >
                                </label>
                            </form>
                            <div class="panel-foot panel-foot-split">
                                <div class="foot-side">
                                    <form method="post" action="/api/send_test_mail">
                                        <button class="quiet" type="submit">(t(lang, Key::SendTestMail))</button>
                                    </form>
                                    match (&test_refusal, &sender.test) {
                                        (Some(refusal), _) => <span class="field-error">(refusal.message_in(lang))</span>,
                                        (None, Some(result)) => match (&result.error, &result.took) {
                                            (Some(problem), _) => <span class="field-error">(crate::i18n::not_delivered_label(lang, &result.moment, problem))</span>,
                                            (None, Some(took)) => <span class="field-note">(crate::i18n::delivered_in_label(lang, took, &result.moment))</span>,
                                            (None, None) => "",
                                        },
                                        (None, None) => "",
                                    }
                                </div>
                                <div class="foot-side">
                                    if let Some(refusal) = &sender_refusal {
                                        <span class="field-error">(refusal.message_in(lang))</span>
                                    }
                                    if sender_saved {
                                        <span class="field-note">(t(lang, Key::Saved))</span>
                                    }
                                    <button class="primary" type="submit" form="sender-settings">(t(lang, Key::Save))</button>
                                </div>
                            </div>
                        </div>
                    </section>
                }

                if section == Section::Limits && let Some((attachment_limit_mb, photo_limit_mb)) = limits {
                    <section class="panel" id="limits">
                        <div class="panel-head">
                            <h2 class="panel-title">(t(lang, Key::WorkspaceLimits))</h2>
                            <span class="chip chip-admin">(t(lang, Key::AdminOnly))</span>
                        </div>
                        <form method="post" action="/api/save_limits" class="panel-body">
                            <div class="field-row">
                                <label class="field">
                                    <span class="field-label">(t(lang, Key::AttachmentLimitLabel))</span>
                                    <input
                                        class="field-input"
                                        type="number"
                                        name="attachment_limit_mb"
                                        min="1"
                                        max=(WIDEST_ATTACHMENT_MB.to_string())
                                        value=(attachment_limit_mb.to_string())
                                        required=""
                                    >
                                </label>
                                <label class="field">
                                    <span class="field-label">(t(lang, Key::PhotoLimitLabel))</span>
                                    <input
                                        class="field-input"
                                        type="number"
                                        name="photo_limit_mb"
                                        min="1"
                                        max=(WIDEST_PHOTO_MB.to_string())
                                        value=(photo_limit_mb.to_string())
                                        required=""
                                    >
                                </label>
                            </div>
                            <label class="field">
                                <span class="field-label">(t(lang, Key::AllowedFileTypesLabel))</span>
                                <input
                                    class="field-input"
                                    type="text"
                                    name="allowed_file_types"
                                    value=(allowed_types.join(", "))
                                    placeholder="png, jpg, pdf, zip"
                                >
                            </label>
                            <div class="panel-foot">
                                if let Some(refusal) = &limits_refusal {
                                    <span class="field-error">(refusal.message_in(lang))</span>
                                }
                                if limits_saved {
                                    <span class="field-note">(t(lang, Key::Saved))</span>
                                }
                                <button class="primary" type="submit">(t(lang, Key::Save))</button>
                            </div>
                        </form>
                    </section>
                }

                if section == Section::Members && let Some(members) = &members {
                    <section class="panel" id="members">
                        <div class="panel-head">
                            <h2 class="panel-title">(t(lang, Key::Members))</h2>
                            <span class="chip chip-admin">(t(lang, Key::AdminOnly))</span>
                        </div>
                        <div class="panel-body">
                            <table class="member-table">
                                <thead>
                                    <tr>
                                        <th>(t(lang, Key::NameCol))</th>
                                        <th>(t(lang, Key::AddressCol))</th>
                                        <th>(t(lang, Key::RoleCol))</th>
                                        <th>(t(lang, Key::AccountCol))</th>
                                    </tr>
                                </thead>
                                <tbody>
                                    for member in members {
                                        let account = if member.is_owner {
                                            t(lang, Key::OwnerStatus).to_string()
                                        } else if !member.has_password {
                                            t(lang, Key::InvitedStatus).to_string()
                                        } else if let Some(day) = member.last_signed_in.clone() {
                                            crate::i18n::last_seen_label(lang, &day)
                                        } else {
                                            t(lang, Key::ActiveStatus).to_string()
                                        };
                                        let member_person = izlek_core::board::Person {
                                            id: member.id.clone(),
                                            display_name: member.display_name.clone(),
                                            has_photo: member.has_photo,
                                        };
                                        <tr class="member-row">
                                            <td class="member-name">
                                                <span class="member-name-row">
                                                    (crate::layout::avatar(cx, &member_person, "avatar-sm").await?)
                                                    (member.display_name.clone())
                                                    if member.is_you {
                                                        <span class="member-you">(t(lang, Key::You))</span>
                                                    }
                                                </span>
                                            </td>
                                            <td class="member-address">(member.email.clone())</td>
                                            <td>
                                                if member.is_owner || member.is_you {
                                                    <span class="chip chip-role">(member.role.as_str().to_string())</span>
                                                } else {
                                                    <form method="post" action="/api/set_role" class="member-role status-form">
                                                        <input type="hidden" name="user_id" value=(member.id.clone())>
                                                        <select class="status-select" name="role" data-autosubmit="">
                                                            <option value="member" selected=(member.role == izlek_core::Role::Member)>(t(lang, Key::RoleMemberOption))</option>
                                                            <option value="viewer" selected=(member.role == izlek_core::Role::Viewer)>(t(lang, Key::RoleViewerOption))</option>
                                                            <option value="admin" selected=(member.role == izlek_core::Role::Admin)>(t(lang, Key::RoleAdminOption))</option>
                                                        </select>
                                                            (crate::detail::glyph::chevron(cx).await?)
                                                    </form>
                                                }
                                            </td>
                                            <td class="member-account">
                                                <span class="member-account-row">
                                                    <span class="member-status">(account)</span>
                                                    if !member.has_password {
                                                        <form method="post" action="/api/resend_link" class="member-resend">
                                                            <input type="hidden" name="user_id" value=(member.id.clone())>
                                                            <button class="quiet" type="submit">(t(lang, Key::ResendMail))</button>
                                                        </form>
                                                    }
                                                </span>
                                            </td>
                                        </tr>
                                    }
                                </tbody>
                            </table>

                            <form method="post" action="/api/invite_member" class="member-invite">
                                <label class="field">
                                    <span class="field-label">(t(lang, Key::NameCol))</span>
                                    <input class="field-input" type="text" name="display_name" maxlength="80" required="">
                                </label>
                                <label class="field">
                                    <span class="field-label">(t(lang, Key::AddressCol))</span>
                                    <input class="field-input" type="email" name="email" required="">
                                </label>
                                <label class="field field-role">
                                    <span class="field-label">(t(lang, Key::RoleCol))</span>
                                    <select class="field-input" name="role">
                                        <option value="member">(t(lang, Key::RoleMemberOption))</option>
                                        <option value="viewer">(t(lang, Key::RoleViewerOption))</option>
                                        <option value="admin">(t(lang, Key::RoleAdminOption))</option>
                                    </select>
                                </label>
                                <button class="primary" type="submit">(t(lang, Key::AddMember))</button>
                            </form>

                            if let Some(refusal) = &member_refusal {
                                <p class="field-error">(refusal.message_in(lang))</p>
                            }
                            if let Some(address) = &mailed {
                                <p class="field-note">(crate::i18n::mailed_to_label(lang, address))</p>
                            }
                        </div>
                    </section>
                }

                if section == Section::Message && let Some(members) = &members {
                    <section class="panel" id="message">
                        <div class="panel-head">
                            <h2 class="panel-title">(t(lang, Key::Message))</h2>
                            <span class="chip chip-admin">(t(lang, Key::AdminOnly))</span>
                        </div>
                        <form method="post" action="/api/send_message" class="panel-body">
                            <label class="field">
                                <span class="field-label">(t(lang, Key::Recipient))</span>
                                <select class="field-input" name="to" data-search="">
                                    <option value="everyone">(t(lang, Key::Everyone))</option>
                                    for member in members {
                                        <option value=(member.id.clone())>(member.display_name.clone())</option>
                                    }
                                </select>
                            </label>
                            <label class="field">
                                <span class="field-label">(t(lang, Key::Subject))</span>
                                <input class="field-input" type="text" name="subject" maxlength="200">
                            </label>
                            <label class="field">
                                <span class="field-label">(t(lang, Key::Body))</span>
                                <textarea class="detail-textarea" name="body" rows="6"></textarea>
                            </label>
                            <div class="panel-foot">
                                if let Some(refusal) = &message_refusal {
                                    <span class="field-error">(refusal.message_in(lang))</span>
                                }
                                if message_saved {
                                    <span class="field-note">(t(lang, Key::Saved))</span>
                                }
                                <button class="primary" type="submit">(t(lang, Key::Send))</button>
                            </div>
                        </form>
                    </section>
                }
            </main>
        </div>
        (crate::dropdown::dropdown_script(cx).await?)
        (crate::layout::escape_script(cx).await?)
    }
}
