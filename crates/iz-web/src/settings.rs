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

use topcoat::Result;
use topcoat::context::Cx;
use topcoat::router::content::Form;
use topcoat::router::{HeaderMap, HeaderValue, StatusCode, header, page, route};
use topcoat::view::view;

use iz_core::detail::ActivityKind;
use iz_core::store::{NewSender, SenderCheck, SenderTest, Store, StoreError, User};

use crate::i18n::{Key, Lang, t};
use crate::server::{Refusal, config, mail, require_admin, require_user, store};

/// The widest the limit may be set to. A ceiling of any size is a promise
/// the disk has to keep, and a limit typed with one extra zero is a mistake
/// nobody notices until the disk is full.
///
/// `iz-topcoat::files` carries its own copy until that lane imports this
/// one (per the settings lane's note there).
pub const WIDEST_ATTACHMENT_MB: u64 = 500;
/// The longest a notification may be held waiting for the rest of its
/// workflow. Two hours is already far past the point where a mail is news.
pub const LONGEST_BATCH_MINUTES: u32 = 120;

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

#[cfg(test)]
mod codec_tests {
    use super::{parse_types, took_label};
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
        "save_sender" | "send_test_mail" | "check_sender" => "outgoing",
        "save_limits" => "limits",
        "set_role" | "set_disabled" | "add_member" => "members",
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
    allowed_file_types: String,
    mail_batch_minutes: u32,
    #[serde(default)]
    reminder_minutes: Option<String>,
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
        || input.attachment_limit_mb > WIDEST_ATTACHMENT_MB
        || input.mail_batch_minutes > LONGEST_BATCH_MINUTES
    {
        return Ok(saved_or_refused("save_limits", Some(Refusal::BadLimit)));
    }
    let reminder_minutes = match input.reminder_minutes.as_deref().map(str::trim) {
        // Absent or empty keeps what the workspace already had: the field
        // rides an older form's post, and switching reminders off is a
        // choice made with a zero, not a blank.
        None | Some("") => match store(cx).workspace().await {
            Ok(Some(workspace)) => workspace.reminder_minutes,
            Ok(None) => 0,
            Err(problem) => {
                eprintln!("store error: {problem}");
                return Ok(saved_or_refused("save_limits", Some(Refusal::Unavailable)));
            }
        },
        Some(raw) => match raw.parse::<u32>() {
            Ok(minutes) => minutes,
            Err(_) => return Ok(saved_or_refused("save_limits", Some(Refusal::BadLimit))),
        },
    };
    let Some(types) = parse_types(&input.allowed_file_types) else {
        return Ok(saved_or_refused("save_limits", Some(Refusal::BadFileType)));
    };
    let outcome = store(cx)
        .set_limits(
            &admin.workspace_id,
            input.attachment_limit_mb * MB,
            &types,
            input.mail_batch_minutes,
            reminder_minutes,
        )
        .await;
    let refusal = match outcome {
        Ok(()) => {
            let _ = store(cx)
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
    // and `https://iz.sh//?task=X` is a link that works by luck.
    let public_url = input.public_url.trim().trim_end_matches('/').to_string();
    // Not typed is not the same as blanked. Nothing here can clear a stored
    // password; replacing it means typing a new one.
    let password = (!input.password.is_empty()).then_some(input.password);

    let store = store(cx).clone();
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
    if outcome.is_ok() {
        // The settings just changed, so anything known about the old server is
        // about a server that is no longer configured. Ask again at once,
        // rather than leaving the panel unchecked until somebody presses a
        // button.
        probe_sender(
            mail(cx),
            store.clone(),
            admin.workspace_id.clone(),
        );
    }
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
/// pointed at an address somebody typed would be a way to make İz mail a
/// stranger on demand, which is a thing worth not building.
/// How long to wait for a mail server before writing the attempt off.
///
/// Long enough for a slow but healthy server on a bad link, short enough that
/// an admin who mistyped the port is told while still looking at the screen.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Asks the mail server whether it would take mail from us, and writes down
/// what it said.
///
/// Nothing is sent to anybody: this is a handshake — connect, TLS, hello,
/// authenticate, hang up. Spawned rather than awaited by its callers so that
/// saving the settings stays as fast as saving anything else; the result
/// announces itself on the live channel, so the panel catches up on its own a
/// moment later without anybody reloading.
fn probe_sender(
    engine: crate::server::Mail,
    store: std::sync::Arc<dyn Store>,
    workspace_id: String,
) {
    tokio::spawn(async move {
        // This is the bound that actually does the work, not the transport's
        // own. Measured: a socket that accepts the connection and then never
        // says a word is NOT caught by lettre's `timeout` — the probe sat
        // there until this fired. A probe that never records leaves the panel
        // on "Unchecked" forever, with nothing to tell anybody it is stuck, so
        // something outside the library has to be the backstop.
        let dialled = tokio::time::timeout(PROBE_TIMEOUT, engine.check()).await;
        let outcome = match dialled {
            Ok(outcome) => outcome,
            Err(_) => Some(Err(iz_core::MailError::retryable(
                "the mail server did not answer",
            ))),
        };
        let Some(outcome) = outcome else {
            // No engine in this process. Nothing to say, and a failure written
            // down here would be about this build rather than these settings.
            return;
        };
        let at = time::OffsetDateTime::now_utc();
        let check = match outcome {
            Ok(took) => SenderCheck {
                at,
                took_ms: took.whole_milliseconds().max(0) as u64,
                error: None,
            },
            Err(problem) => SenderCheck {
                at,
                took_ms: 0,
                // Built from what the server said, never from what we sent it.
                error: Some(problem.message.clone()),
            },
        };
        if let Err(problem) = store.record_sender_check(&workspace_id, check).await {
            eprintln!("store error: {problem}");
        }
    });
}

/// Dials the mail server without sending anything, on an admin's say-so.
#[route(POST "/api/check_sender")]
async fn check_sender(cx: &Cx) -> Result<(StatusCode, HeaderMap, Vec<u8>)> {
    let admin = match require_admin(cx).await {
        Ok(admin) => admin,
        Err(refusal) => return Ok(saved_or_refused("check_sender", Some(refusal))),
    };
    let store = store(cx).clone();
    probe_sender(mail(cx), store.clone(), admin.workspace_id.clone());
    let _ = store
        .record_event(
            Some(&admin.id),
            &ActivityKind::Other("sender_checked".to_string()),
            "",
            time::OffsetDateTime::now_utc(),
        )
        .await;
    Ok(saved_or_refused("check_sender", None))
}

#[route(POST "/api/send_test_mail")]
async fn send_test_mail(cx: &Cx) -> Result<(StatusCode, HeaderMap, Vec<u8>)> {
    let admin = match require_admin(cx).await {
        Ok(admin) => admin,
        Err(refusal) => return Ok(saved_or_refused("send_test_mail", Some(refusal))),
    };
    let lang = Lang::from_code(&admin.language);

    let store = store(cx).clone();
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
    timezone: String,
    theme: String,
    language: String,
    ui: String,
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

/// Writes the person asking's display-only preferences. Nobody touches
/// anybody else here: the id comes from the session, never from the form.
/// Name and address are im's to vouch and are not written here at all.
#[route(POST "/api/save_profile")]
async fn save_profile(
    cx: &Cx,
    Form(input): Form<SaveProfileForm>,
) -> Result<(StatusCode, HeaderMap, Vec<u8>)> {
    let user = match require_user(cx).await {
        Ok(user) => user,
        Err(refusal) => return Ok(saved_or_refused("save_profile", Some(refusal))),
    };
    if !zone_options().contains(&input.timezone) {
        return Ok(saved_or_refused("save_profile", Some(Refusal::BadZone)));
    }
    if !THEME_OPTIONS.contains(&input.theme.as_str()) {
        return Ok(saved_or_refused("save_profile", Some(Refusal::BadTheme)));
    }
    if !UI_OPTIONS.contains(&input.ui.as_str()) {
        return Ok(saved_or_refused("save_profile", Some(Refusal::BadUi)));
    }
    if !LANGUAGE_OPTIONS.contains(&input.language.as_str()) {
        return Ok(saved_or_refused("save_profile", Some(Refusal::BadLanguage)));
    }
    let store = store(cx);
    let refusal = match store
        .set_preferences(
            &user.id,
            &input.timezone,
            &input.theme,
            &input.language,
            &input.ui,
        )
        .await
    {
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
        Err(problem) => {
            eprintln!("store error: {problem}");
            Some(Refusal::Unavailable)
        }
    };
    Ok(saved_or_refused("save_profile", refusal))
}

#[derive(serde::Deserialize)]
struct SetRoleForm {
    user_id: String,
    role: iz_core::Role,
}

/// Changes a member's role between member and viewer. Admin-only. The admin
/// role itself is im's to grant and is never written here; the owner's row
/// and the caller's own row are refused rather than acted on.
#[route(POST "/api/set_role")]
async fn set_role(
    cx: &Cx,
    Form(input): Form<SetRoleForm>,
) -> Result<(StatusCode, HeaderMap, Vec<u8>)> {
    let admin = match require_admin(cx).await {
        Ok(admin) => admin,
        Err(refusal) => return Ok(saved_or_refused("set_role", Some(refusal))),
    };
    if input.role == iz_core::Role::Admin {
        return Ok(saved_or_refused("set_role", Some(Refusal::Forbidden)));
    }
    let store = store(cx);
    let member = match store.user(&input.user_id).await? {
        Some(member) => member,
        None => return Ok(saved_or_refused("set_role", Some(Refusal::NoSuchMember))),
    };
    if member.workspace_id != admin.workspace_id || member.id == admin.id {
        return Ok(saved_or_refused("set_role", Some(Refusal::Forbidden)));
    }
    let owner = store.owner().await?;
    if owner.map(|owner| owner.id) == Some(member.id.clone()) {
        return Ok(saved_or_refused("set_role", Some(Refusal::Forbidden)));
    }
    let refusal = match store.set_role(&member.id, input.role).await {
        Ok(()) => {
            let _ = store
                .record_event(
                    Some(&admin.id),
                    &ActivityKind::RoleChanged,
                    &format!("{} -> {}", member.display_name, input.role.as_str()),
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
    Ok(saved_or_refused("set_role", refusal))
}

#[derive(serde::Deserialize)]
struct DisableForm {
    user_id: String,
    #[serde(default)]
    disabled: Option<String>,
}

/// Disables or re-enables one account. Disabling yourself is refused — the
/// workspace must never be left with no way back in through its only admin.
/// A disabled account signs in to nothing: the guards read it as signed out.
#[route(POST "/api/set_disabled")]
async fn set_disabled(
    cx: &Cx,
    Form(input): Form<DisableForm>,
) -> Result<(StatusCode, HeaderMap, Vec<u8>)> {
    let admin = match require_admin(cx).await {
        Ok(admin) => admin,
        Err(refusal) => return Ok(saved_or_refused("set_disabled", Some(refusal))),
    };
    if input.user_id == admin.id {
        return Ok(saved_or_refused("set_disabled", Some(Refusal::Forbidden)));
    }
    let disabled = match input.disabled.as_deref() {
        None => true,
        Some(value) => !matches!(
            value.trim().to_lowercase().as_str(),
            "" | "0" | "false" | "no" | "off"
        ),
    };
    let refusal = match store(cx).set_user_disabled(&input.user_id, disabled).await {
        Ok(()) => None,
        Err(problem) => {
            eprintln!("store error: {problem}");
            Some(Refusal::Unavailable)
        }
    };
    Ok(saved_or_refused("set_disabled", refusal))
}

#[derive(serde::Deserialize)]
struct AddMemberForm {
    display_name: String,
    email: String,
    role: iz_core::Role,
}

/// Adds a member who has not signed in yet. Admin-only. The row is
/// unclaimed — no sub, never signed in — until the address signs in and
/// [`Store::provision_user`](iz_core::store::Store::provision_user) claims
/// it. The admin role is im's to grant and is refused here, like in
/// `set_role`.
#[route(POST "/api/add_member")]
async fn add_member(
    cx: &Cx,
    Form(input): Form<AddMemberForm>,
) -> Result<(StatusCode, HeaderMap, Vec<u8>)> {
    let admin = match require_admin(cx).await {
        Ok(admin) => admin,
        Err(refusal) => return Ok(saved_or_refused("add_member", Some(refusal))),
    };
    if input.role == iz_core::Role::Admin {
        return Ok(saved_or_refused("add_member", Some(Refusal::Forbidden)));
    }
    if input.display_name.trim().is_empty() {
        return Ok(saved_or_refused("add_member", Some(Refusal::EmptyName)));
    }
    if !is_address(input.email.trim()) {
        return Ok(saved_or_refused("add_member", Some(Refusal::BadEmail)));
    }
    let refusal = match store(cx)
        .add_member(
            &admin.workspace_id,
            &input.email,
            &input.display_name,
            input.role,
        )
        .await
    {
        Ok(_) => None,
        Err(StoreError::Conflict("member")) => Some(Refusal::AlreadyMember),
        Err(problem) => {
            eprintln!("store error: {problem}");
            Some(Refusal::Unavailable)
        }
    };
    Ok(saved_or_refused("add_member", refusal))
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
        return Ok(saved_or_refused(
            "send_message",
            Some(Refusal::EmptySubject),
        ));
    }
    let body = input.body.trim();
    if body.is_empty() {
        return Ok(saved_or_refused("send_message", Some(Refusal::EmptyBody)));
    }
    let store = store(cx).clone();
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
            None => {
                return Ok(saved_or_refused(
                    "send_message",
                    Some(Refusal::NoSuchMember),
                ));
            }
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

/// One row of the member list, as an admin may see it. Identity is im's to
/// vouch — the row carries no password and no link token, only what the
/// workspace itself decides: the role and whether the account is disabled.
struct Member {
    id: String,
    display_name: String,
    email: String,
    role: iz_core::Role,
    disabled: bool,
    /// The day they last signed in, as the list writes it, or nothing.
    last_signed_in: Option<String>,
    is_you: bool,
    /// The first account. It administers the workspace and cannot be
    /// removed.
    is_owner: bool,
}

async fn members_now(cx: &Cx, asking: &User) -> Result<Vec<Member>> {
    let store = store(cx);
    let owner = store.owner().await?.map(|owner| owner.id);
    let users = store.users(&asking.workspace_id).await?;
    let zone = iz_core::detail::parse_zone(&asking.timezone);
    Ok(users
        .into_iter()
        .map(|user| Member {
            disabled: user.disabled,
            last_signed_in: user
                .last_signed_in_at
                .map(|at| iz_core::board::day_label(at.to_offset(zone).date())),
            is_you: user.id == asking.id,
            is_owner: owner.as_deref() == Some(user.id.as_str()),
            id: user.id,
            display_name: user.display_name,
            email: user.email,
            role: user.role,
        })
        .collect())
}

async fn limits_now(cx: &Cx, workspace_id: &str) -> Result<(u64, Vec<String>, u32, u32)> {
    let workspace = store(cx)
        .workspace()
        .await?
        .filter(|workspace| workspace.id == workspace_id)
        .ok_or_else(|| topcoat::Error::from(std::io::Error::other("no workspace")))?;
    Ok((
        workspace.attachment_limit_bytes / MB,
        workspace.allowed_file_types,
        workspace.mail_batch_minutes,
        workspace.reminder_minutes,
    ))
}

/// What the row under the test button says, once the workspace holds a
/// `sender_test` to read.
struct TestResult {
    moment: String,
    took: Option<String>,
    error: Option<String>,
}

/// The last handshake, rendered.
struct CheckResult {
    moment: String,
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
    /// How the last handshake went, and when. `None` means nobody has asked
    /// the server yet since these settings were saved.
    check: Option<CheckResult>,
    /// The origin mailed links point at, empty when the configured one is
    /// still in use.
    public_url: String,
}

/// What the panel says about the mail server, in the order the states happen.
///
/// The point of four states rather than two is that "the fields are filled in"
/// and "the server let us in" are different facts, and the old chip said
/// `Connected` for the first. A host typed correctly with the wrong password
/// showed green while every mail was refused.
#[derive(PartialEq, Eq)]
enum Standing {
    /// Something the sender needs is missing.
    NotConfigured,
    /// Complete, but nobody has asked the server yet.
    Unchecked,
    /// The server accepted our credentials, at this moment.
    Connected(String),
    /// The server would not have us, in its own words.
    Refused(String),
}

impl Sender {
    /// Whether the form holds everything a sender needs. Says nothing about
    /// whether any of it is right — see [`Standing`].
    fn is_complete(&self) -> bool {
        !self.host.trim().is_empty()
            && !self.username.trim().is_empty()
            && !self.from_address.trim().is_empty()
            && self.password_set
    }

    fn standing(&self) -> Standing {
        if !self.is_complete() {
            return Standing::NotConfigured;
        }
        match &self.check {
            None => Standing::Unchecked,
            Some(check) => match &check.error {
                Some(said) => Standing::Refused(said.clone()),
                None => Standing::Connected(check.moment.clone()),
            },
        }
    }
}

async fn sender_now(cx: &Cx, zone: time::UtcOffset) -> Result<Sender> {
    let workspace = store(cx).workspace().await?;
    Ok(match workspace {
        None => Sender {
            host: String::new(),
            port: 587,
            username: String::new(),
            from_name: String::new(),
            from_address: String::new(),
            password_set: false,
            test: None,
            check: None,
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
                moment: iz_core::detail::moment_label_in(test.at, zone),
                took: test.error.is_none().then(|| took_label(test.took_ms)),
                error: test.error,
            }),
            check: workspace.sender_check.map(|check| CheckResult {
                moment: iz_core::detail::moment_label_in(check.at, zone),
                error: check.error,
            }),
            public_url: workspace.public_url.unwrap_or_default(),
        },
    })
}

/// Whether a typed address is an origin a link can be built on: an http or
/// https scheme, a host after it, and no whitespace anywhere. A path is
/// allowed — İz behind `example.com/iz` is a real deployment — but a
/// bare host or a mail address is not.
fn is_origin(value: &str) -> bool {
    let Some(rest) = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
    else {
        return false;
    };
    let host = rest.split('/').next().unwrap_or_default();
    !host.is_empty() && !host.contains('@') && !value.chars().any(char::is_whitespace)
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

    let zone = iz_core::detail::parse_zone(&user.timezone);
    // What a link falls back to when the field is empty — the placeholder
    // shows it rather than an invented example.
    let configured_url = config(cx).listen_url();
    let sender = if administers {
        Some(sender_now(cx, zone).await?)
    } else {
        None
    };
    let (limits, allowed_types) = if administers {
        let (attachment, types, batch, reminder) =
            limits_now(cx, &user.workspace_id).await?;
        (Some((attachment, batch, reminder)), types)
    } else {
        (None, Vec::new())
    };
    let members = if administers {
        Some(members_now(cx, &user).await?)
    } else {
        None
    };

    let (profile_refusal, profile_saved) = call_state(query, "save_profile");
    let (sender_refusal, sender_saved) = call_state(query, "save_sender");
    let (test_refusal, _) = call_state(query, "send_test_mail");
    let (limits_refusal, limits_saved) = call_state(query, "save_limits");
    let (role_refusal, _) = call_state(query, "set_role");
    let (disabled_refusal, _) = call_state(query, "set_disabled");
    let (add_refusal, add_saved) = call_state(query, "add_member");
    let member_refusal = role_refusal.or(disabled_refusal).or(add_refusal);
    let (message_refusal, message_saved) = call_state(query, "send_message");

    view! {
        cx =>
        <header class="topbar">
            (crate::layout::mark(cx).await?)
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
                    <a class=(rail_class(section, Section::Message)) href="/settings?section=message">(t(lang, Key::Message))</a>
                }
            </nav>
            <main class="settings-stage">
                if section == Section::Profile {
                <section class="panel" id="profile">
                    <div class="panel-head">
                        <h2 class="panel-title">(t(lang, Key::YourProfile))</h2>
                    </div>
                    <div class="panel-body">
                        <div class="identity-row">
                            (crate::layout::avatar(cx, &user.id, &user.display_name, "avatar-lg").await?)
                            <div class="identity-who">
                                <div class="identity-name">(user.display_name.clone())</div>
                                <div class="identity-address">(user.email.clone())</div>
                            </div>
                        </div>
                        <p class="panel-lede">(t(lang, Key::IdentityFromIm))</p>
                    </div>
                    <form method="post" action="/api/save_profile" class="panel-body">
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
                </section>
                }

                if section == Section::Outgoing && let Some(sender) = &sender {
                    let standing = sender.standing();
                    let complete = sender.is_complete();
                    let password_set = sender.password_set;
                    // One chip, four states. `Connected` carries the moment the
                    // server said so, because the claim is about a moment: a
                    // password rotated since is not something this can know.
                    let (chip_class, chip_text) = match &standing {
                        Standing::NotConfigured => {
                            ("chip chip-off", t(lang, Key::NotConfiguredChip).to_string())
                        }
                        Standing::Unchecked => {
                            ("chip chip-off", t(lang, Key::Unchecked).to_string())
                        }
                        Standing::Connected(at) => (
                            "chip chip-connected",
                            format!("{} {}", t(lang, Key::Connected), at),
                        ),
                        Standing::Refused(_) => {
                            ("chip chip-off", t(lang, Key::Refused).to_string())
                        }
                    };
                    <section class="panel" id="outgoing">
                        <div class="panel-head">
                            <h2 class="panel-title">(t(lang, Key::OutgoingMail))</h2>
                            <span class="chip chip-admin">(t(lang, Key::AdminOnly))</span>
                            <span class=(chip_class)>(chip_text)</span>
                        </div>
                        <div class="panel-body">
                            // The server's own words when it refused us:
                            // "535 authentication failed" is something an admin
                            // can act on, and a bare chip is not.
                            if let Standing::Refused(said) = &standing {
                                <p class="panel-lede">(said)</p>
                            }
                            if !complete {
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
                                            placeholder="İz"
                                        >
                                    </label>
                                    <label class="field">
                                        <span class="field-label">(t(lang, Key::FromAddressLabel))</span>
                                        <input
                                            class="field-input"
                                            type="text"
                                            name="from_address"
                                            value=(sender.from_address.clone())
                                            placeholder="board@iz.sh"
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
                                    <form method="post" action="/api/check_sender">
                                        <button class="quiet" type="submit" disabled=(!complete)>
                                            (t(lang, Key::CheckConnection))
                                        </button>
                                    </form>
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

                if section == Section::Limits
                    && let Some((attachment_limit_mb, mail_batch_minutes, reminder_minutes)) = limits
                {
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
                            </div>
                            <div class="field-row">
                                <label class="field">
                                    <span class="field-label">(t(lang, Key::MailBatchLabel))</span>
                                    <input
                                        class="field-input"
                                        type="number"
                                        name="mail_batch_minutes"
                                        min="0"
                                        max=(LONGEST_BATCH_MINUTES.to_string())
                                        value=(mail_batch_minutes.to_string())
                                        required=""
                                    >
                                </label>
                                <label class="field">
                                    <span class="field-label">(t(lang, Key::ReminderMinutesLabel))</span>
                                    <input
                                        class="field-input"
                                        type="number"
                                        name="reminder_minutes"
                                        min="0"
                                        value=(reminder_minutes.to_string())
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
                            // The register keeps its named columns at every width; on a
                            // narrow screen this box pans it sideways, silently, instead
                            // of letting the columns crush the role select out.
                            <div class="table-pan">
                            <table class="member-table">
                                <thead>
                                    <tr>
                                        <th class="member-col-name">(t(lang, Key::NameCol))</th>
                                        <th class="member-col-address">(t(lang, Key::AddressCol))</th>
                                        <th class="member-col-role">(t(lang, Key::RoleCol))</th>
                                        <th class="member-col-account">(t(lang, Key::AccountCol))</th>
                                    </tr>
                                </thead>
                                <tbody>
                                    for member in members {
                                        let account = if member.is_owner {
                                            t(lang, Key::OwnerStatus).to_string()
                                        } else if member.disabled {
                                            t(lang, Key::DisabledBadge).to_string()
                                        } else if let Some(day) = member.last_signed_in.clone() {
                                            crate::i18n::last_seen_label(lang, &day)
                                        } else {
                                            t(lang, Key::ActiveStatus).to_string()
                                        };
                                        <tr class="member-row">
                                            <td class="member-col-name member-name">
                                                <span class="member-name-row">
                                                    (crate::layout::avatar(cx, &member.id, &member.display_name, "avatar-sm").await?)
                                                    <a href=(format!("/people/{}", member.id))>(member.display_name.clone())</a>
                                                    if member.is_you {
                                                        <span class="member-you">(t(lang, Key::You))</span>
                                                    }
                                                    if member.disabled {
                                                        <span class="chip chip-off">(t(lang, Key::DisabledBadge))</span>
                                                    }
                                                </span>
                                            </td>
                                            <td class="member-col-address member-address">(member.email.clone())</td>
                                            <td class="member-col-role">
                                                // The admin role is im's to grant: an im-admin row
                                                // wears its badge and is never written here. The
                                                // owner's row and one's own stay read-only the way
                                                // they always were.
                                                if member.is_owner || member.is_you || member.role == iz_core::Role::Admin {
                                                    <span class="chip chip-role">(t(lang, match member.role {
                                                        iz_core::Role::Admin => Key::RoleAdminOption,
                                                        iz_core::Role::Member => Key::RoleMemberOption,
                                                        iz_core::Role::Viewer => Key::RoleViewerOption,
                                                    }))</span>
                                                } else {
                                                    <form method="post" action="/api/set_role" class="member-role status-form">
                                                        <input type="hidden" name="user_id" value=(member.id.clone())>
                                                        <select class="status-select" name="role" data-autosubmit="">
                                                            <option value="member" selected=(member.role == iz_core::Role::Member)>(t(lang, Key::RoleMemberOption))</option>
                                                            <option value="viewer" selected=(member.role == iz_core::Role::Viewer)>(t(lang, Key::RoleViewerOption))</option>
                                                        </select>
                                                            (crate::detail::glyph::chevron(cx).await?)
                                                    </form>
                                                }
                                            </td>
                                            <td class="member-col-account member-account">
                                                <span class="member-account-row">
                                                    <span class="member-status">(account)</span>
                                                    // Not for yourself: your own sign-in is yours to
                                                    // keep, and the guards would read you as signed
                                                    // out from under your own hand.
                                                    if !member.is_you {
                                                    <form method="post" action="/api/set_disabled" class="member-resend">
                                                        <input type="hidden" name="user_id" value=(member.id.clone())>
                                                        if member.disabled {
                                                            <input type="hidden" name="disabled" value="0">
                                                            <button class="quiet" type="submit">(t(lang, Key::EnableUser))</button>
                                                        } else {
                                                            <input type="hidden" name="disabled" value="1">
                                                            <button class="quiet quiet-danger" type="submit">(t(lang, Key::DisableUser))</button>
                                                        }
                                                    </form>
                                                    }
                                                </span>
                                            </td>
                                        </tr>
                                    }
                                </tbody>
                            </table>
                            </div>

                            if let Some(refusal) = &member_refusal {
                                <p class="field-error">(refusal.message_in(lang))</p>
                            }
                            <form method="post" action="/api/add_member">
                                <div class="field-row">
                                    <label class="field">
                                        <span class="field-label">(t(lang, Key::NameLabel))</span>
                                        <input class="field-input" type="text" name="display_name" maxlength="200">
                                    </label>
                                    <label class="field">
                                        <span class="field-label">(t(lang, Key::EmailLabel))</span>
                                        <input class="field-input" type="text" name="email" maxlength="320">
                                    </label>
                                    <label class="field field-narrow">
                                        <span class="field-label">(t(lang, Key::RoleCol))</span>
                                        <select class="field-input" name="role">
                                            <option value="member">(t(lang, Key::RoleMemberOption))</option>
                                            <option value="viewer">(t(lang, Key::RoleViewerOption))</option>
                                        </select>
                                    </label>
                                </div>
                                <div class="panel-foot">
                                    if add_saved {
                                        <span class="field-note">(t(lang, Key::Saved))</span>
                                    }
                                    <button class="primary" type="submit">(t(lang, Key::AddMember))</button>
                                </div>
                            </form>
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
        (crate::detail::escape_closes(cx).await?)
    }
}
