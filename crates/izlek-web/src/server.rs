//! Server-side plumbing for the auth surface: the session cookie, the person
//! behind the current request, and the role guards.
//!
//! Every guard here is the real one. The UI hides what a role may not do, but
//! the answer that matters is the one given in this module, on the server.
//!
//! Ported from `izlek-web/src/server.rs` and the `Refusal`/`call_id` pieces of
//! `izlek-web/src/auth.rs`, onto topcoat's context/cookie/layer primitives.

use izlek_core::accounts::{AccountError, Accounts};
use izlek_core::auth::PasswordProblem;
use izlek_core::board::Transition;
use izlek_core::mail::Engine;
use izlek_core::store::{Freeing, User};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use topcoat::asset::AssetBundle;
use topcoat::context::{Cx, app_context, memoize, try_app_context};
use topcoat::cookie::{Cookie, Cookies, SameSite, cookie, cookies};
use topcoat::router::request::headers;
use topcoat::router::{Body, HeaderValue, Next, StatusCode, header, response::Response, to_bytes};

/// The session cookie's name. One cookie, one browser, one session row.
pub const SESSION_COOKIE: &str = "izlek_session";

/// The workspace's account service, put into context by the router.
pub fn accounts(cx: &Cx) -> Accounts {
    app_context::<Accounts>(cx).clone()
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

    /// Hands a recorded activity to the engine, off the request, the same way
    /// `after` hands a transition.
    ///
    /// The activity row is already written by the time this is called; this
    /// rehydrates it and lets the engine decide whether any rule owes a mail.
    /// `store` is the same store the request already wrote through — the
    /// engine has no public handle back to it, so the caller carries it.
    pub fn after_activity(
        &self,
        store: std::sync::Arc<dyn izlek_core::store::Store>,
        activity_id: String,
    ) {
        let Some(engine) = self.0.clone() else {
            return;
        };
        tokio::spawn(async move {
            let ev = store.event(&activity_id).await;
            let report = match ev {
                Ok(Some(izlek_core::store::Event::Happened(ev))) => {
                    eprintln!("AFTER_ACTIVITY happened kind={:?}", ev.kind);
                    match engine.on_activity(&ev).await {
                        Ok(_) => {
                            engine
                                .deliver_owed(time::OffsetDateTime::now_utc(), 8)
                                .await
                        }
                        Err(problem) => Err(problem),
                    }
                }
                Ok(_) => return,
                Err(problem) => Err(problem),
            };
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

    /// Asks the mail server whether it would have us, sending nothing.
    pub async fn check(&self) -> Option<Result<time::Duration, izlek_core::MailError>> {
        let engine = self.0.clone()?;
        Some(engine.check_sender().await)
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
pub fn mail(cx: &Cx) -> Mail {
    try_app_context::<Mail>(cx)
        .cloned()
        .unwrap_or_else(Mail::silent)
}

/// The application cookie jar, with the attributes every İzlek cookie wants.
fn app_cookies(cx: &Cx) -> impl Cookies {
    cookies(cx)
        .default_secure(true)
        .default_http_only(true)
        .default_same_site(SameSite::Lax)
        .default_path("/")
}

/// The cookie value this request presented, if it presented one.
pub fn presented_session(cx: &Cx) -> Option<String> {
    cookies(cx)
        .get(SESSION_COOKIE)
        .map(|c| c.value().to_string())
}

/// Any other cookie the request presented, by name — the client-set,
/// non-`HttpOnly` ones (e.g. `izlek_rows_<section>`) that JS reads and
/// writes on its own.
pub fn presented_cookie(cx: &Cx, name: &str) -> Option<String> {
    cookies(cx).get(name).map(|c| c.value().to_string())
}

/// A stable-enough label for the client, for rate limiting. A proxy header is
/// only trusted because İzlek is meant to sit behind one; the address bucket is
/// the limit that actually protects the Argon2 work either way.
///
/// topcoat 0.6.2 exposes no peer address; x-forwarded-for or nothing.
pub fn client_label(cx: &Cx) -> String {
    let Some(forwarded) = headers(cx).get("x-forwarded-for") else {
        return "unknown".to_string();
    };
    let Ok(raw) = forwarded.to_str() else {
        return "unknown".to_string();
    };
    match raw.split(',').next() {
        Some(first) if !first.trim().is_empty() => first.trim().to_string(),
        _ => "unknown".to_string(),
    }
}

/// The stylesheet this binary serves must be the one compiled into it.
///
/// `asset!` embeds an asset's declaration — its id and source path — into
/// the binary, but the served bytes live in the bundle directory beside the
/// executable, and topcoat loads whatever manifest it finds there. A bundle
/// left behind by another deploy sits beside a newer binary without a word
/// of complaint, and the pages then reference a stylesheet whose bytes are
/// from another generation — the mixed generation a browser once caught on
/// production. `build.rs` stamps the compiled stylesheet's SHA-256 into the
/// binary; this hashes the bundle's bytes against it, so a foreign bundle
/// refuses the boot instead of serving under it.
///
/// Returns the startup log line naming the served fingerprint, or the
/// reason the boot must not proceed.
pub fn stylesheet_guard(bundle: &AssetBundle) -> Result<String, String> {
    let expected = env!("IZLEK_STYLE_FINGERPRINT");
    let stylesheet = bundle
        .catalog()
        .assets()
        .find(|asset| {
            let name = asset.name();
            name.starts_with("main-") && name.ends_with(".css")
        })
        .ok_or_else(|| {
            format!(
                "the asset bundle at {} carries no stylesheet",
                bundle.dir().display()
            )
        })?;
    let bytes = std::fs::read(bundle.dir().join(stylesheet.name())).map_err(|err| {
        format!(
            "the bundled stylesheet {} could not be read: {err}",
            stylesheet.name()
        )
    })?;
    let actual = format!("sha256:{:x}", sha2::Sha256::digest(&bytes));
    if actual != expected {
        return Err(format!(
            "the asset bundle at {} is from another build: stylesheet {} is {actual} but this binary was compiled against {expected}; run `topcoat asset bundle` and redeploy",
            bundle.dir().display(),
            stylesheet.name()
        ));
    }
    Ok(format!(
        "assets  stylesheet {} ({actual})",
        stylesheet.name()
    ))
}

/// Writes the session cookie. `HttpOnly` so script cannot read it, `Secure` so
/// it never crosses plain HTTP, `SameSite=Lax` so another site's form cannot
/// post with it.
pub fn set_session_cookie(cx: &Cx, token: &str, lifetime: time::Duration) {
    app_cookies(cx).add(cookie! {
        SESSION_COOKIE = token.to_owned();
        Path = "/";
        Secure;
        HttpOnly;
        SameSite = Lax;
        MaxAge = lifetime
    });
}

/// Removes the session cookie from this browser. The server-side revocation is
/// what actually ends the session; this only tidies the client.
pub fn clear_session_cookie(cx: &Cx) {
    app_cookies(cx).remove(Cookie::build((SESSION_COOKIE, "")).path("/").build());
}

/// The person behind this request, or nobody, or the store failing to say.
///
/// A store error is not "nobody" — a busy database mid-drag is a fact about
/// the database, not about whether this browser is signed in, and a caller
/// that folded the two together would send a signed-in person to a sign-in
/// screen because a write elsewhere held a lock for a moment.
#[memoize(as_ref)]
pub async fn current_user(cx: &Cx) -> Result<Option<User>, AccountError> {
    let Some(presented) = presented_session(cx) else {
        return Ok(None);
    };
    accounts(cx).authenticate(&presented).await
}

/// The person behind this request, or a refusal the caller can return as-is.
///
/// A store error becomes `Refusal::Unavailable` — the same place every other
/// account error already lands, via `refusal_for` below — rather than
/// `SignInFirst`, which would tell a signed-in person to sign in again over
/// what was only a database hiccup.
pub async fn require_user(cx: &Cx) -> Result<User, Refusal> {
    match current_user(cx).await {
        Ok(Some(user)) => Ok(user.clone()),
        Ok(None) => Err(Refusal::SignInFirst),
        Err(error) => Err(refusal_for(error)),
    }
}

/// The admin behind this request. A member or a viewer is refused *here*, not
/// merely hidden from in the UI.
pub async fn require_admin(cx: &Cx) -> Result<User, Refusal> {
    let user = require_user(cx).await?;
    if user.role.can_administer() {
        Ok(user)
    } else {
        Err(Refusal::Forbidden)
    }
}

/// The person behind this request if they may change the board.
pub async fn require_writer(cx: &Cx) -> Result<User, Refusal> {
    let user = require_user(cx).await?;
    if user.role.can_write_tasks() {
        Ok(user)
    } else {
        Err(Refusal::Forbidden)
    }
}

/// Shared by `From<AccountError>` and the `&AccountError` `current_user`
/// hands back once memoized, so both paths land on the exact same wording.
fn refusal_for(error: &AccountError) -> Refusal {
    match error {
        AccountError::Rejected => Refusal::Rejected,
        AccountError::RateLimited => Refusal::RateLimited,
        AccountError::Password(problem) => Refusal::Password(*problem),
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

impl From<AccountError> for Refusal {
    fn from(error: AccountError) -> Self {
        refusal_for(&error)
    }
}

/// Everything a refused call is allowed to say.
///
/// Ported from `izlek-web/src/auth.rs`; the auth pages that construct most of
/// these variants land in a later slice, but the server plumbing above needs
/// the type now.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Refusal {
    /// Wrong address, wrong password, or no account at all — deliberately one
    /// answer for all three.
    Rejected,
    RateLimited,
    /// A password broke a stated rule. Only reachable once we know who the
    /// person is, so it gives nothing away. The problem itself rides in the
    /// variant — the wording is built at the edge, in the reader's language,
    /// never reflected out of the address bar.
    Password(PasswordProblem),
    Forbidden,
    SignInFirst,
    AlreadyClaimed,
    /// Admin-side wording, shown on the member list and nowhere public.
    AddressTaken,
    /// The link is spent, expired, or was never real.
    LinkNotUsable,
    /// A card with no title is not a card.
    EmptyTitle,
    /// A person with no name is not a person the board can show.
    EmptyName,
    /// A limit of nothing, or a limit wider than the disk should promise.
    BadLimit,
    /// The timezone field was not one of the offsets the form offers.
    BadZone,
    /// The theme field was not one of the values the form offers.
    BadTheme,
    /// The ui field was not one of the values the form offers.
    BadUi,
    /// The language field was not one of the values the form offers.
    BadLanguage,
    /// The email field did not look like an address.
    BadEmail,
    /// Something in the allowed-types list is not a file extension.
    BadFileType,
    /// The sender panel was saved with a field it cannot work without, or with
    /// one that is not what it claims to be. The message names the field: this
    /// is a form somebody is filling in, not an attacker probing, and "that did
    /// not work" would send them round the panel guessing.
    BadSender(String),
    /// A comment with nothing in it.
    EmptyComment,
    /// The upload arrived without a file in it.
    NoFile,
    /// The bytes kept coming past what this workspace allows per file.
    FileTooBig,
    /// The name does not end in one of the extensions the admin allows.
    FileTypeNotAllowed,
    /// The bytes uploaded as a profile photo do not sniff as an image.
    NotAnImage,
    /// A rule with no subject line: the mail it sends would arrive blank.
    EmptySubject,
    /// A tag with no name is not a tag the board can show.
    EmptyTag,
    /// A tag with cards on it stays. Emptying it is the admin's job, and it
    /// is one they can see the size of.
    TagInUse,
    /// A message with no body: there is nothing for the recipient to read.
    EmptyBody,
    /// The date field did not hold a date.
    BadDeadline,
    /// The moment field's time box did not hold a 24h `HH:MM`, or a time
    /// arrived with no day to sit on.
    BadClock,
    /// The link asked for would put a task behind itself.
    Cycle,
    /// The card was already moved out of the column this request thought it
    /// was in, by somebody else, while this person was deciding.
    MovedAlready,
    /// A task headed for a done column while its subtasks are not done. The
    /// count is deliberately not in the sentence: the parent's card already
    /// carries it, and a number cannot survive the round trip through the
    /// address bar that a browser without script makes.
    SubtasksOpen,
    /// Subtasks go one level deep: the task named as a parent is itself a
    /// part, or the task being filed already has parts of its own.
    NotNestable,
    /// No such task — or none this account may see. Deliberately one answer for
    /// both.
    NotFound,
    /// A recipient id that names nobody in this workspace.
    NoSuchMember,
    Unavailable,
}

impl Refusal {
    /// The refusal in words, ported verbatim from `izlek-web/src/auth.rs`.
    pub fn message(&self) -> String {
        match self {
            Refusal::Rejected => "That did not work.".to_string(),
            Refusal::RateLimited => {
                "Too many attempts — wait a few minutes and try again.".to_string()
            }
            Refusal::Password(problem) => problem.to_string(),
            Refusal::Forbidden => "Not permitted.".to_string(),
            Refusal::SignInFirst => "Sign in first.".to_string(),
            Refusal::AlreadyClaimed => "This workspace already has an owner.".to_string(),
            Refusal::AddressTaken => "That address already has an account.".to_string(),
            Refusal::LinkNotUsable => "This link no longer works.".to_string(),
            Refusal::EmptyTitle => "Give the task a title.".to_string(),
            Refusal::EmptyName => "Give yourself a name.".to_string(),
            Refusal::BadLimit => {
                "A limit has to be at least 1 MB, and no wider than 500 MB per file or 20 MB per photo."
                    .to_string()
            }
            Refusal::BadZone => "That is not a timezone.".to_string(),
            Refusal::BadTheme => "That is not a theme.".to_string(),
            Refusal::BadUi => "That is not an interface.".to_string(),
            Refusal::BadLanguage => "That is not a language.".to_string(),
            Refusal::BadEmail => "That is not an address.".to_string(),
            Refusal::BadFileType => {
                "File types are extensions — png, pdf, zip — separated by commas.".to_string()
            }
            Refusal::BadSender(problem) => problem.clone(),
            Refusal::EmptyComment => "Write something first.".to_string(),
            Refusal::NoFile => "Choose a file first.".to_string(),
            Refusal::FileTooBig => "Too big for this workspace.".to_string(),
            Refusal::FileTypeNotAllowed => {
                "That kind of file is not on this workspace's allowed list.".to_string()
            }
            Refusal::NotAnImage => "That is not an image.".to_string(),
            Refusal::EmptySubject => "Give the rule a subject line.".to_string(),
            Refusal::EmptyTag => "Give the tag a name.".to_string(),
            Refusal::TagInUse => "This tag still has cards.".to_string(),
            Refusal::EmptyBody => "Write something first.".to_string(),
            Refusal::BadDeadline => "That is not a date.".to_string(),
            Refusal::BadClock => "That is not a time.".to_string(),
            Refusal::Cycle => "That link would put this task behind itself.".to_string(),
            Refusal::MovedAlready => "Somebody moved this card first.".to_string(),
            Refusal::SubtasksOpen => "Subtasks are still open.".to_string(),
            Refusal::NotNestable => "Subtasks go one level deep.".to_string(),
            Refusal::NotFound => "No such task.".to_string(),
            Refusal::NoSuchMember => "No such member.".to_string(),
            Refusal::Unavailable => "Something went wrong.".to_string(),
        }
    }

    /// `message()`, in a user's language, for the handful of refusals
    /// `board.rs`/`detail.rs` render — those two pages' only refusal variants.
    /// Everything else falls back to the English `message()`: the rest of the
    /// app's pages have not been translated yet.
    pub fn message_in(&self, lang: crate::i18n::Lang) -> String {
        use crate::i18n::Lang::Tr;
        if lang != Tr {
            return self.message();
        }
        match self {
            Refusal::Forbidden => "İzin verilmiyor.".to_string(),
            Refusal::SignInFirst => "Önce oturum aç.".to_string(),
            Refusal::EmptyTitle => "Göreve bir başlık ver.".to_string(),
            Refusal::EmptyComment => "Önce bir şey yaz.".to_string(),
            Refusal::NoFile => "Önce bir dosya seç.".to_string(),
            Refusal::FileTooBig => "Bu çalışma alanı için çok büyük.".to_string(),
            Refusal::FileTypeNotAllowed => {
                "Bu dosya türü bu çalışma alanının izin verdiği listede değil.".to_string()
            }
            Refusal::NotAnImage => "Bu bir resim değil.".to_string(),
            Refusal::BadDeadline => "Bu bir tarih değil.".to_string(),
            Refusal::BadClock => "Bu bir saat değil.".to_string(),
            Refusal::Cycle => "Bu bağlantı görevi kendi arkasına koyar.".to_string(),
            Refusal::MovedAlready => "Bu kartı başka biri zaten taşıdı.".to_string(),
            Refusal::SubtasksOpen => "Alt görevler hâlâ açık.".to_string(),
            Refusal::NotNestable => "Alt görevler tek seviyedir.".to_string(),
            Refusal::NotFound => "Böyle bir görev yok.".to_string(),
            Refusal::NoSuchMember => "Böyle bir üye yok.".to_string(),
            Refusal::Unavailable => "Bir şeyler ters gitti.".to_string(),
            Refusal::BadLimit => {
                "Limit en az 1 MB, dosya başına en çok 500 MB, fotoğraf başına en çok 20 MB olabilir."
                    .to_string()
            }
            Refusal::BadZone => "Bu bir saat dilimi değil.".to_string(),
            Refusal::BadTheme => "Bu bir tema değil.".to_string(),
            Refusal::BadUi => "Bu bir arayüz değil.".to_string(),
            Refusal::BadLanguage => "Bu bir dil değil.".to_string(),
            Refusal::BadEmail => "Bu bir adres değil.".to_string(),
            Refusal::BadFileType => {
                "Dosya türleri virgülle ayrılmış uzantılardır — png, pdf, zip.".to_string()
            }
            // `BadSender`'s sentence is built where the complaint is
            // decided (`settings.rs`), already in the caller's language —
            // this is only reached for a request with no admin to read a
            // language off of, so it falls back to English like the rest.
            Refusal::BadSender(_) => self.message(),
            Refusal::EmptySubject => "Kurala bir konu ver.".to_string(),
            Refusal::EmptyTag => "Etikete bir ad ver.".to_string(),
            Refusal::TagInUse => "Bu etikette kartlar var.".to_string(),
            Refusal::EmptyBody => "Önce bir şey yaz.".to_string(),
            Refusal::Rejected => "Bu işe yaramadı.".to_string(),
            Refusal::RateLimited => {
                "Çok fazla deneme — birkaç dakika bekleyip tekrar dene.".to_string()
            }
            Refusal::AlreadyClaimed => "Bu çalışma alanının zaten bir sahibi var.".to_string(),
            Refusal::AddressTaken => "Bu adresin zaten bir hesabı var.".to_string(),
            Refusal::LinkNotUsable => "Bu bağlantı artık çalışmıyor.".to_string(),
            Refusal::EmptyName => "Kendine bir isim ver.".to_string(),
            // The validator's own words, in the reader's language.
            Refusal::Password(problem) => match problem {
                PasswordProblem::TooShort => "En az 10 karakter.".to_string(),
                PasswordProblem::LooksLikeYou => "Adresin ya da adın değil.".to_string(),
                PasswordProblem::IsCurrent => "Bu zaten mevcut parolan.".to_string(),
            },
        }
    }

    /// The refusal a `code` names, or nothing. Nothing for an unknown word: the
    /// query is whatever the address bar holds, so a code that is not one of
    /// ours says nothing at all rather than something invented.
    pub fn from_code(code: &str) -> Option<Refusal> {
        Some(match code {
            "rejected" => Refusal::Rejected,
            "rate-limited" => Refusal::RateLimited,
            "password-short" => Refusal::Password(PasswordProblem::TooShort),
            "password-you" => Refusal::Password(PasswordProblem::LooksLikeYou),
            "password-current" => Refusal::Password(PasswordProblem::IsCurrent),
            "forbidden" => Refusal::Forbidden,
            "sign-in-first" => Refusal::SignInFirst,
            "already-claimed" => Refusal::AlreadyClaimed,
            "address-taken" => Refusal::AddressTaken,
            "link-not-usable" => Refusal::LinkNotUsable,
            "empty-title" => Refusal::EmptyTitle,
            "empty-name" => Refusal::EmptyName,
            "bad-limit" => Refusal::BadLimit,
            "bad-zone" => Refusal::BadZone,
            "bad-theme" => Refusal::BadTheme,
            "bad-ui" => Refusal::BadUi,
            "bad-language" => Refusal::BadLanguage,
            "bad-email" => Refusal::BadEmail,
            "bad-file-type" => Refusal::BadFileType,
            // The specific complaint (which field, and why) lives only in the
            // response the save itself returned; a code that survived a round
            // trip through the address bar carries no more than this.
            "bad-sender" => Refusal::BadSender("That sender setting will not work.".to_string()),
            "empty-comment" => Refusal::EmptyComment,
            "no-file" => Refusal::NoFile,
            "file-too-big" => Refusal::FileTooBig,
            "file-type" => Refusal::FileTypeNotAllowed,
            "not-an-image" => Refusal::NotAnImage,
            "empty-subject" => Refusal::EmptySubject,
            "empty-tag" => Refusal::EmptyTag,
            "tag-in-use" => Refusal::TagInUse,
            "empty-body" => Refusal::EmptyBody,
            "bad-deadline" => Refusal::BadDeadline,
            "bad-clock" => Refusal::BadClock,
            "cycle" => Refusal::Cycle,
            "moved-already" => Refusal::MovedAlready,
            "subtasks-open" => Refusal::SubtasksOpen,
            "not-nestable" => Refusal::NotNestable,
            "not-found" => Refusal::NotFound,
            "no-such-member" => Refusal::NoSuchMember,
            "unavailable" => Refusal::Unavailable,
            _ => return None,
        })
    }

    /// The refusal as a short word, for the address bar.
    ///
    /// A browser without script never sees a call's return value: it posts the
    /// form, follows the redirect, and the page it lands on has to be told what
    /// happened. That telling goes through the query, so every refusal needs a
    /// name that survives a round trip through a URL.
    pub fn code(&self) -> &'static str {
        match self {
            Refusal::Rejected => "rejected",
            Refusal::RateLimited => "rate-limited",
            // The problem itself, not the wording, so the sentence is built
            // here on the way back rather than reflected out of the address.
            Refusal::Password(problem) => match problem {
                PasswordProblem::TooShort => "password-short",
                PasswordProblem::LooksLikeYou => "password-you",
                PasswordProblem::IsCurrent => "password-current",
            },
            Refusal::Forbidden => "forbidden",
            Refusal::SignInFirst => "sign-in-first",
            Refusal::AlreadyClaimed => "already-claimed",
            Refusal::AddressTaken => "address-taken",
            Refusal::LinkNotUsable => "link-not-usable",
            Refusal::EmptyTitle => "empty-title",
            Refusal::EmptyName => "empty-name",
            Refusal::BadLimit => "bad-limit",
            Refusal::BadZone => "bad-zone",
            Refusal::BadTheme => "bad-theme",
            Refusal::BadUi => "bad-ui",
            Refusal::BadLanguage => "bad-language",
            Refusal::BadEmail => "bad-email",
            Refusal::BadFileType => "bad-file-type",
            Refusal::BadSender(_) => "bad-sender",
            Refusal::EmptyComment => "empty-comment",
            Refusal::NoFile => "no-file",
            Refusal::FileTooBig => "file-too-big",
            Refusal::FileTypeNotAllowed => "file-type",
            Refusal::NotAnImage => "not-an-image",
            Refusal::EmptySubject => "empty-subject",
            Refusal::EmptyTag => "empty-tag",
            Refusal::TagInUse => "tag-in-use",
            Refusal::EmptyBody => "empty-body",
            Refusal::BadDeadline => "bad-deadline",
            Refusal::BadClock => "bad-clock",
            Refusal::Cycle => "cycle",
            Refusal::MovedAlready => "moved-already",
            Refusal::SubtasksOpen => "subtasks-open",
            Refusal::NotNestable => "not-nestable",
            Refusal::NotFound => "not-found",
            Refusal::NoSuchMember => "no-such-member",
            Refusal::Unavailable => "unavailable",
        }
    }
}

/// Which call a carried refusal belongs to. Two forms on one page must not both
/// claim the same sentence, so the redirect names the call and this is the name:
/// the last piece of the server function's path, with anything that is not a
/// plain word dropped so it can only ever be compared, never rendered.
pub fn call_id(path: &str) -> String {
    path.rsplit('/')
        .next()
        .unwrap_or(path)
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect()
}

/// What a call refused with, however the browser without script carried it
/// back — read straight off the query, since there is no client-side action
/// value to check first the way a hydrated page in the old UI had.
///
/// Ported from the old UI's `auth.rs` `refusal_of`/`refusal_from_query`,
/// collapsed into one function: every topcoat page is rendered server-side on
/// every request, so there is only ever the query to read.
pub fn refusal_of(cx: &Cx, call: &str) -> Option<Refusal> {
    let query = topcoat::router::request::uri(cx).query()?;
    let mut code = None;
    let mut on = None;
    for pair in query.split('&') {
        if let Some((key, value)) = pair.split_once('=') {
            match key {
                "refusal" => code = Some(value),
                "on" => on = Some(value),
                _ => {}
            }
        }
    }
    if on? != call {
        return None;
    }
    Refusal::from_code(code?)
}

/// The page a form was posted from: the `Referer`, with the answer any
/// earlier post left on its query dropped, or `nowhere` when no `Referer`
/// came with the request.
///
/// The feedback pairs — `refusal=`, `on=`, `why=`, `saved=` — are how a page
/// renders what the last post did. Sending the browser back with them still
/// on the query re-renders that old answer under the new post's own: a change
/// that succeeded right after one that was refused announces the refusal
/// again, and reads as having failed. The answer this redirect carries — the
/// pairs it was built with, or the body [`carry_refusal_on_redirect`] copies
/// onto the query — is the one that shows; an earlier one never survives the
/// trip.
pub fn back_to(cx: &Cx, nowhere: &str) -> String {
    let referer = headers(cx)
        .get(header::REFERER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or(nowhere);
    let (path, query) = referer.split_once('?').unwrap_or((referer, ""));
    let pairs: Vec<&str> = query
        .split('&')
        .filter(|pair| {
            !pair.is_empty()
                && !["refusal", "on", "why", "saved"].iter().any(|key| {
                    pair.strip_prefix(*key)
                        .is_some_and(|rest| rest.starts_with('='))
                })
        })
        .collect();
    if pairs.is_empty() {
        return path.to_string();
    }
    format!("{path}?{}", pairs.join("&"))
}

/// Puts a refusal on the redirect a browser without script follows.
///
/// A hydrated page reads the call's return value straight off the action. A
/// browser without script has no such thing: it posts the form, the server
/// function handler answers with a redirect back to the page it came from, and
/// the value — the whole refusal — sits in a body nobody will ever look at.
/// The click then looks like nothing happening, which is the worst answer
/// İzlek can give.
///
/// So the refusal is copied onto the `Location`, as `?refusal=<code>&on=<call>`,
/// and the page renders it from the query. This is one place rather than
/// thirty-eight because the shape is the same for every refusing call, present
/// and future: nothing here knows what any of them do.
///
/// Requests carrying script are untouched — they are answered with the value
/// itself and never see a redirect.
///
/// Ported from axum's `middleware::from_fn` onto a topcoat `#[layer]`. topcoat's
/// Post/Redirect/Get helper (`error::see_other`) answers with `303 See Other`,
/// not the `302 Found` axum's server-function redirect used, so the status this
/// checks for is `SEE_OTHER` — the guard's three conditions are otherwise
/// unchanged.
#[topcoat::router::layer("/api")]
async fn carry_refusal_on_redirect(
    cx: &Cx,
    body: Body,
    next: Next<'_>,
) -> topcoat::Result<Response> {
    // A form post from a browser asks for a page back. A server-function call
    // from the hydrated bundle does not.
    let wants_page = headers(cx)
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("text/html"));
    let called = call_id(topcoat::router::request::uri(cx).path());
    let has_referer = headers(cx).contains_key(header::REFERER);
    let response = next.run(cx, body).await?;
    if !wants_page || !has_referer || response.status() != StatusCode::SEE_OTHER {
        return Ok(response);
    }

    let (mut parts, body) = response.into_parts();
    // The body of one of these redirects is a serialised `Option<Refusal>` and
    // nothing else; the cap is there so a response that is something else
    // entirely cannot be read into memory whole. A body that fails to parse —
    // an empty one included, the shape a route with nothing to say back sends
    // — is read as "no refusal", never as "leave the Location alone": the
    // Referer sanitization below has to run on every redirect this layer
    // sees, not only the ones that happen to carry a refusal.
    let Ok(bytes) = to_bytes(body, 64 * 1024).await else {
        return Ok(Response::from_parts(parts, Body::empty()));
    };
    let refusal = serde_json::from_slice::<Option<Refusal>>(&bytes)
        .ok()
        .flatten();
    if let Some(location) = parts
        .headers
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
    {
        let rewritten = match refusal {
            Some(refusal) => carrying(location, refusal.code(), &called),
            None => Some(same_origin(location).to_string()),
        };
        if let Some(carried) = rewritten
            && let Ok(value) = HeaderValue::from_str(&carried)
        {
            parts.headers.insert(header::LOCATION, value);
        }
    }
    Ok(Response::from_parts(parts, Body::from(bytes)))
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
    // browser back there would make İzlek an open redirect, so the address is
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

/// The cache directives a response cannot choose for itself.
///
/// HTML is revalidated on every load: a page the browser kept from before a
/// deploy is the old app answering under the new one's address, which is how a
/// fixed bug once came back after shipping. So every response whose body is a
/// document gets `no-cache`. Responses that already carry a directive keep it —
/// topcoat stamps the fingerprinted assets under `/_topcoat/assets` with a year
/// of `immutable` and the live stream with `no-cache`, and the photo and
/// attachment handlers stamp their own — and everything else (redirects, JSON
/// answers) ships none, the idiom they already ride on: no directive, nothing
/// cached. One layer at `/`, because the decision is about what the bytes are,
/// not which route built them.
#[topcoat::router::layer("/")]
async fn cache_directives(cx: &Cx, body: Body, next: Next<'_>) -> topcoat::Result<Response> {
    let response = next.run(cx, body).await?;
    let (mut parts, body) = response.into_parts();
    let is_html = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/html"));
    if !is_html || parts.headers.contains_key(header::CACHE_CONTROL) {
        return Ok(Response::from_parts(parts, body));
    }
    parts
        .headers
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    Ok(Response::from_parts(parts, body))
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
    // reaches this function as the Location. İzlek answers on its own ground or
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
        // is sent somewhere on İzlek.
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

#[cfg(test)]
mod refusal_message_tests {
    use super::Refusal;
    use izlek_core::auth::PasswordProblem;

    #[test]
    fn every_refusal_survives_the_address_bar() {
        let all = [
            Refusal::Rejected,
            Refusal::RateLimited,
            Refusal::Password(PasswordProblem::TooShort),
            Refusal::Password(PasswordProblem::LooksLikeYou),
            Refusal::Password(PasswordProblem::IsCurrent),
            Refusal::Forbidden,
            Refusal::SignInFirst,
            Refusal::AlreadyClaimed,
            Refusal::AddressTaken,
            Refusal::LinkNotUsable,
            Refusal::EmptyTitle,
            Refusal::EmptyComment,
            Refusal::NoFile,
            Refusal::FileTooBig,
            Refusal::FileTypeNotAllowed,
            Refusal::BadDeadline,
            Refusal::BadClock,
            Refusal::BadZone,
            Refusal::BadTheme,
            Refusal::BadLanguage,
            Refusal::BadEmail,
            Refusal::Cycle,
            Refusal::MovedAlready,
            Refusal::SubtasksOpen,
            Refusal::NotNestable,
            Refusal::NotFound,
            Refusal::NoSuchMember,
            Refusal::TagInUse,
            Refusal::Unavailable,
        ];
        for refusal in all {
            assert_eq!(
                Refusal::from_code(refusal.code()).as_ref(),
                Some(&refusal),
                "{} did not come back as itself",
                refusal.code()
            );
        }
    }

    #[test]
    fn an_unknown_code_says_nothing() {
        assert_eq!(Refusal::from_code("not-a-refusal"), None);
        assert_eq!(Refusal::from_code(""), None);
    }
}

#[cfg(test)]
mod refusal_of_tests {
    use super::{Cx, Refusal, refusal_of};
    use topcoat::context::CxTestBuilder;

    fn cx_at(uri: &str) -> Cx {
        let (parts, ()) = http::Request::builder()
            .uri(uri)
            .body(())
            .unwrap()
            .into_parts();
        CxTestBuilder::new().request_context(parts).build()
    }

    #[test]
    fn reads_a_matching_refusal_off_the_query() {
        let cx = cx_at("/?refusal=cycle&on=sign_in");
        assert_eq!(refusal_of(&cx, "sign_in"), Some(Refusal::Cycle));
    }

    #[test]
    fn a_refusal_for_another_call_is_not_this_one() {
        let cx = cx_at("/?refusal=cycle&on=link_tasks");
        assert_eq!(refusal_of(&cx, "sign_in"), None);
    }

    #[test]
    fn no_query_carries_nothing() {
        let cx = cx_at("/");
        assert_eq!(refusal_of(&cx, "sign_in"), None);
    }
}

#[cfg(test)]
mod call_id_tests {
    use super::call_id;

    #[test]
    fn a_call_is_named_by_its_last_path_piece() {
        assert_eq!(call_id("/api/link_tasks"), "link_tasks");
        assert_eq!(call_id("/api/link_tasks?x=1"), "link_tasksx1");
    }
}

#[cfg(test)]
mod session_cookie_tests {
    use super::{SESSION_COOKIE, clear_session_cookie, presented_session, set_session_cookie};
    use topcoat::context::{Cx, CxTestBuilder};
    use topcoat::cookie::{CookieJarCell, write_cookies};

    /// A `Cx` whose request carried the given `Cookie` header, ready for
    /// `set_session_cookie`/`presented_session`/`clear_session_cookie` to read
    /// and write through.
    fn cx_with(cookie_header: Option<&str>) -> Cx {
        let mut builder = http::Request::builder();
        if let Some(value) = cookie_header {
            builder = builder.header(http::header::COOKIE, value);
        }
        let (parts, ()) = builder.body(()).unwrap().into_parts();
        CxTestBuilder::new()
            .request_context(parts)
            .request_context(CookieJarCell::new())
            .build()
    }

    fn set_cookie_headers(cx: &Cx) -> Vec<String> {
        let mut headers = http::HeaderMap::new();
        write_cookies(cx, &mut headers);
        headers
            .get_all(http::header::SET_COOKIE)
            .iter()
            .map(|v| v.to_str().unwrap().to_owned())
            .collect()
    }

    /// The cookie name is a wire contract: every existing browser's `Cookie`
    /// header still says "izlek_session", and it must stay byte-identical or
    /// every one of them is signed out at once.
    #[test]
    fn the_cookie_name_stayed_byte_identical() {
        assert_eq!(SESSION_COOKIE, "izlek_session");
    }

    #[test]
    fn setting_writes_the_session_cookie_with_the_right_attributes() {
        let cx = cx_with(None);
        set_session_cookie(&cx, "tok123", time::Duration::days(14));

        let set = set_cookie_headers(&cx);
        assert_eq!(set.len(), 1);
        assert!(set[0].starts_with("izlek_session=tok123;"), "{}", set[0]);
        for attr in [
            "Path=/",
            "Secure",
            "HttpOnly",
            "SameSite=Lax",
            "Max-Age=1209600",
        ] {
            assert!(set[0].contains(attr), "{} missing {attr}", set[0]);
        }
    }

    #[test]
    fn presented_session_reads_the_cookie_the_browser_sent() {
        let cx = cx_with(Some("izlek_session=tok123"));
        assert_eq!(presented_session(&cx).as_deref(), Some("tok123"));
    }

    #[test]
    fn absent_cookie_presents_nothing() {
        let cx = cx_with(None);
        assert_eq!(presented_session(&cx), None);
    }

    #[test]
    fn clearing_expires_the_cookie_at_the_same_path() {
        let cx = cx_with(Some("izlek_session=tok123"));
        clear_session_cookie(&cx);

        let set = set_cookie_headers(&cx);
        assert_eq!(set.len(), 1);
        assert!(set[0].starts_with("izlek_session="), "{}", set[0]);
        assert!(set[0].contains("Max-Age=0"), "{}", set[0]);
        assert!(set[0].contains("Path=/"), "{}", set[0]);
    }
}
