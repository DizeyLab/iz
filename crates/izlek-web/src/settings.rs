//! Settings, from the Settings and MemberSettings artboards.
//!
//! Which panels a person gets is decided on the server: the sender and the
//! limits belong to an admin, and a Member's answer simply does not carry them
//! — the page cannot hide what it was never sent. Every mutation here checks
//! the role again in its own handler, because a panel that is not drawn is a
//! courtesy and not a guard.

use leptos::prelude::*;
use serde::{Deserialize, Serialize};

use crate::auth::{Me, Refusal};

/// The sender as the admin's screen may see it.
///
/// The password is not a field here and there is no field it could be put in.
/// The store keeps it in a column the workspace read path never selects, and
/// the only reader is the mailer. What the screen gets instead is
/// `password_set`: enough to say "set" beside the field, and enough to know
/// that a save which sends no password is an edit rather than a deletion.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sender {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub from_name: String,
    pub from_address: String,
    pub password_set: bool,
    /// The last test result, as the panel prints it, or nothing when the button
    /// has not been pressed since the sender was last edited.
    pub test: Option<TestResult>,
}

/// What the row under the test button says.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestResult {
    /// `Aug 25 10:41`, in UTC, the same stamp the activity strip uses.
    pub moment: String,
    /// `1.2 s`, or nothing when the send never got as far as being timed.
    pub took: Option<String>,
    /// What the mail server said, if it refused.
    pub error: Option<String>,
}

impl Sender {
    /// Whether this is a sender at all, or an empty panel waiting to be filled
    /// in. Mail waits, rather than fails, until this is true.
    pub fn is_connected(&self) -> bool {
        !self.host.trim().is_empty()
            && !self.username.trim().is_empty()
            && !self.from_address.trim().is_empty()
            && self.password_set
    }
}

#[cfg(feature = "ssr")]
impl Sender {
    /// What the screen may see of the workspace's sender.
    pub fn of(workspace: &izlek_core::store::Workspace) -> Self {
        Self {
            host: workspace.smtp_host.clone().unwrap_or_default(),
            // 587 is submission with STARTTLS, which is what a new panel should
            // suggest rather than making somebody look it up.
            port: workspace.smtp_port.and_then(|p| u16::try_from(p).ok()).unwrap_or(587),
            username: workspace.smtp_username.clone().unwrap_or_default(),
            from_name: workspace.smtp_from_name.clone().unwrap_or_default(),
            from_address: workspace.smtp_from_address.clone().unwrap_or_default(),
            password_set: workspace.smtp_password_set,
            test: workspace.sender_test.as_ref().map(|test| TestResult {
                moment: izlek_core::detail::moment_label(test.at),
                took: test.error.is_none().then(|| took_label(test.took_ms)),
                error: test.error.clone(),
            }),
        }
    }
}

/// `1.2 s` for anything over a second, `840 ms` below it. A mail server that
/// answered in under a second is worth saying so precisely; one that took eight
/// is worth a number somebody can compare to the last one.
#[cfg(feature = "ssr")]
fn took_label(ms: u64) -> String {
    if ms >= 1000 {
        format!("{:.1} s", ms as f64 / 1000.0)
    } else {
        format!("{ms} ms")
    }
}

/// The workspace's limits, as the panel edits them. Megabytes here and bytes
/// in the store: the field says "25 MB per file" and a person typing 25 into
/// it should not have to know what that is in bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    pub attachment_limit_mb: u64,
    pub photo_limit_mb: u64,
    /// Extensions, lowercase and without dots. Empty means every type.
    pub allowed_file_types: Vec<String>,
}

/// The widest either limit may be set to. A ceiling of any size is a promise
/// the disk has to keep, and a limit typed with one extra zero is a mistake
/// nobody notices until the disk is full.
pub const WIDEST_ATTACHMENT_MB: u64 = 500;
pub const WIDEST_PHOTO_MB: u64 = 20;

#[cfg(feature = "ssr")]
const MB: u64 = 1024 * 1024;

/// One row of the member list, as an admin may see it. There is no password
/// and no token here: whether a password exists is a fact about the account,
/// and the token of a live link is shown once at the moment it is minted and
/// is never readable afterwards — the store keeps only its hash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub id: String,
    pub display_name: String,
    pub email: String,
    pub role: izlek_core::Role,
    pub has_password: bool,
    /// The day they last signed in, as the list writes it, or nothing.
    pub last_signed_in: Option<String>,
    pub is_you: bool,
    /// The first account. It administers the workspace and cannot be removed.
    pub is_owner: bool,
}

/// One settings screen's worth of state. `sender` is `None` for exactly one
/// reason — the person asking does not administer the workspace — and a
/// workspace with no sender yet carries an empty one, which is the panel an
/// admin fills in. Nobody else is told a host name.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SettingsSnapshot {
    pub me: Me,
    pub administers: bool,
    pub sender: Option<Sender>,
    /// The limits, for the admin who may change them. A Member's answer does
    /// not carry the panel's contents.
    pub limits: Option<Limits>,
    /// The member list, for the admin who holds it. Everyone else gets
    /// nothing — not a hidden table.
    pub members: Option<Vec<Member>>,
}

/// The settings this browser may see.
#[server]
pub async fn current_settings() -> Result<Result<SettingsSnapshot, Refusal>, ServerFnError> {
    use crate::server::require_user;

    let user = match require_user().await {
        Ok(user) => user,
        Err(refusal) => return Ok(Err(refusal)),
    };
    let administers = user.role.can_administer();
    let limits = match administers {
        true => Some(limits_now(&user.workspace_id).await?),
        false => None,
    };
    let members = match administers {
        true => Some(members_now(&user).await?),
        false => None,
    };
    Ok(Ok(SettingsSnapshot {
        me: Me {
            id: user.id,
            display_name: user.display_name,
            email: user.email,
            role: user.role,
        },
        administers,
        // Only an admin is told anything about the sender at all.
        sender: match administers {
            true => Some(sender_now().await?),
            false => None,
        },
        limits,
        members,
    }))
}

#[cfg(feature = "ssr")]
async fn sender_now() -> Result<Sender, ServerFnError> {
    let store = crate::server::accounts().store().clone();
    let workspace = store
        .workspace()
        .await
        .map_err(|e| ServerFnError::new(e.to_string()))?;
    // A workspace that somehow is not there yet reads as an empty panel rather
    // than an error: there is nothing to correct and nothing to alarm anyone
    // with, and the fields are the same ones either way.
    Ok(workspace.as_ref().map(Sender::of).unwrap_or_default())
}

#[cfg(feature = "ssr")]
async fn members_now(asking: &izlek_core::store::User) -> Result<Vec<Member>, ServerFnError> {
    let store = crate::server::accounts().store().clone();
    let owner = store
        .owner()
        .await
        .map_err(|e| ServerFnError::new(e.to_string()))?
        .map(|owner| owner.id);
    let users = store
        .users(&asking.workspace_id)
        .await
        .map_err(|e| ServerFnError::new(e.to_string()))?;
    Ok(users
        .into_iter()
        .map(|user| Member {
            has_password: user.password_hash.is_some(),
            last_signed_in: user.last_signed_in_at.map(|at| izlek_core::board::day_label(at.date())),
            is_you: user.id == asking.id,
            is_owner: owner.as_deref() == Some(user.id.as_str()),
            id: user.id,
            display_name: user.display_name,
            email: user.email,
            role: user.role,
        })
        .collect())
}

/// Sends the same person another first-sign-in link, and hands the admin the
/// address to pass on. Admin-only, checked here.
///
/// The link is returned exactly once, at the moment it is minted: the store
/// keeps only its hash, so no later call — not this one, not a page reload —
/// can produce it again.
#[server]
pub async fn resend_link(user_id: String) -> Result<Result<String, Refusal>, ServerFnError> {
    use crate::server::{accounts, require_admin};

    let admin = match require_admin().await {
        Ok(admin) => admin,
        Err(refusal) => return Ok(Err(refusal)),
    };
    match accounts().resend_invitation(&admin, &user_id).await {
        Ok(invitation) => Ok(Ok(format!("/join/{}", invitation.token.expose()))),
        Err(error) => Ok(Err(error.into())),
    }
}

#[cfg(feature = "ssr")]
async fn limits_now(workspace_id: &str) -> Result<Limits, ServerFnError> {
    let workspace = crate::server::accounts()
        .store()
        .workspace()
        .await
        .map_err(|e| ServerFnError::new(e.to_string()))?
        .filter(|workspace| workspace.id == workspace_id)
        .ok_or_else(|| ServerFnError::new("no workspace"))?;
    Ok(Limits {
        attachment_limit_mb: workspace.attachment_limit_bytes / MB,
        photo_limit_mb: workspace.photo_limit_bytes / MB,
        allowed_file_types: workspace.allowed_file_types,
    })
}

/// Changes the workspace's limits. Admin-only, checked here.
///
/// The list is parsed rather than trusted: it is what a later upload is
/// checked against, so a type that is not a plain extension has no business
/// being stored as one.
#[server]
pub async fn save_limits(
    attachment_limit_mb: u64,
    photo_limit_mb: u64,
    allowed_file_types: String,
) -> Result<Option<Refusal>, ServerFnError> {
    use crate::server::{accounts, require_admin};

    let admin = match require_admin().await {
        Ok(admin) => admin,
        Err(refusal) => return Ok(Some(refusal)),
    };
    if attachment_limit_mb == 0
        || photo_limit_mb == 0
        || attachment_limit_mb > WIDEST_ATTACHMENT_MB
        || photo_limit_mb > WIDEST_PHOTO_MB
    {
        return Ok(Some(Refusal::BadLimit));
    }
    let Some(types) = parse_types(&allowed_file_types) else {
        return Ok(Some(Refusal::BadFileType));
    };
    match accounts()
        .store()
        .set_limits(
            &admin.workspace_id,
            attachment_limit_mb * MB,
            photo_limit_mb * MB,
            &types,
        )
        .await
    {
        Ok(()) => Ok(None),
        Err(problem) => {
            eprintln!("store error: {problem}");
            Ok(Some(Refusal::Unavailable))
        }
    }
}

/// Writes the workspace's sender. Admin-only, checked here.
///
/// `password` is empty when the admin did not type one, and empty means "leave
/// the stored one alone" rather than "delete it": the field is write-only, so
/// the form has nothing to send back for a password that already exists, and a
/// save that took the blank literally would stop the workspace sending mail as
/// a side effect of fixing a typo in the port.
///
/// A sender with no password at all is refused rather than stored, because a
/// half-filled sender is not a configuration — it is a mail nobody receives and
/// a ledger nobody reads.
#[server]
pub async fn save_sender(
    host: String,
    port: u32,
    username: String,
    password: String,
    from_name: String,
    from_address: String,
) -> Result<Option<Refusal>, ServerFnError> {
    use crate::server::{accounts, require_admin};
    use izlek_core::store::NewSender;

    let admin = match require_admin().await {
        Ok(admin) => admin,
        Err(refusal) => return Ok(Some(refusal)),
    };

    let host = host.trim().to_string();
    let username = username.trim().to_string();
    let from_name = from_name.trim().to_string();
    let from_address = from_address.trim().to_string();
    // Not typed is not the same as blanked. Nothing here can clear a stored
    // password; replacing it means typing a new one.
    let password = (!password.is_empty()).then_some(password);

    let store = accounts().store().clone();
    let known = store
        .workspace()
        .await
        .map_err(|e| ServerFnError::new(e.to_string()))?
        .map(|ws| ws.smtp_password_set)
        .unwrap_or(false);

    let complaint = if host.is_empty() {
        Some("Give the SMTP host.")
    } else if host.contains(char::is_whitespace) || host.contains('@') || host.contains('/') {
        Some("The SMTP host is a host name, not an address or a URL.")
    } else if !(1..=65535).contains(&port) {
        Some("A port is a number between 1 and 65535.")
    } else if username.is_empty() {
        Some("Give the SMTP username.")
    } else if password.is_none() && !known {
        Some("A password is needed the first time.")
    } else if !is_address(&from_address) {
        Some("That is not a from-address.")
    } else {
        None
    };
    if let Some(problem) = complaint {
        return Ok(Some(Refusal::BadSender(problem.to_string())));
    }

    match store
        .set_sender(
            &admin.workspace_id,
            NewSender {
                host,
                port,
                username,
                password,
                from_name,
                from_address,
            },
        )
        .await
    {
        Ok(()) => Ok(None),
        Err(problem) => {
            eprintln!("store error: {problem}");
            Ok(Some(Refusal::Unavailable))
        }
    }
}

/// Sends one mail to the admin who pressed the button, and writes down how it
/// went so the answer survives a reload.
///
/// It goes to their own address and nowhere else. A test that could be pointed
/// at an address somebody typed would be a way to make Izlek mail a stranger
/// on demand, which is a thing worth not building.
#[server]
pub async fn send_test_mail() -> Result<Option<Refusal>, ServerFnError> {
    use crate::server::{accounts, mail, require_admin};
    use izlek_core::store::SenderTest;

    let admin = match require_admin().await {
        Ok(admin) => admin,
        Err(refusal) => return Ok(Some(refusal)),
    };

    let store = accounts().store().clone();
    let configured = store
        .workspace()
        .await
        .map_err(|e| ServerFnError::new(e.to_string()))?
        .is_some_and(|ws| ws.smtp_password_set && ws.smtp_host.is_some());
    if !configured {
        return Ok(Some(Refusal::BadSender(
            "Fill the sender in and save it first — there is nothing to test yet.".to_string(),
        )));
    }

    let Some(outcome) = mail().test(&admin.email).await else {
        // No engine in this process at all. Nothing sends here, and saying so
        // beats writing down a failure the settings did not cause.
        return Ok(Some(Refusal::Unavailable));
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
            // The mailer builds this from what the server said, never from the
            // credentials it sent, so it is safe to store and to show.
            error: Some(problem.message.clone()),
        },
    };
    if let Err(problem) = store.record_sender_test(&admin.workspace_id, test).await {
        eprintln!("store error: {problem}");
        return Ok(Some(Refusal::Unavailable));
    }
    Ok(None)
}

/// The shallowest check that catches a typo without rejecting a real address.
/// Nothing here is trying to decide what RFC 5322 permits — the mail server
/// settles that, and a refusal from it lands in the ledger with its own words.
pub fn is_address(raw: &str) -> bool {
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

/// The typed list as extensions, or `None` if one of them is not an extension.
///
/// Lowercased, dots dropped, duplicates dropped, and nothing but letters and
/// digits kept — a "type" with a slash or a dot in it would be a path or a
/// pattern wearing an extension's clothes, and this list is checked against a
/// filename later.
pub fn parse_types(raw: &str) -> Option<Vec<String>> {
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

/// Renames the person asking. Nobody renames anybody else here: the id comes
/// from the session, never from the form.
#[server]
pub async fn save_profile(display_name: String) -> Result<Option<Refusal>, ServerFnError> {
    use crate::server::{accounts, require_user};

    let user = match require_user().await {
        Ok(user) => user,
        Err(refusal) => return Ok(Some(refusal)),
    };
    let display_name = display_name.trim().to_string();
    if display_name.is_empty() {
        return Ok(Some(Refusal::EmptyName));
    }
    match accounts()
        .store()
        .set_profile(&user.id, &display_name, user.photo_path.as_deref())
        .await
    {
        Ok(()) => Ok(None),
        Err(problem) => {
            eprintln!("store error: {problem}");
            Ok(Some(Refusal::Unavailable))
        }
    }
}

/// The settings screen.
#[component]
pub fn SettingsPage() -> impl IntoView {
    let settings = Resource::new(|| (), |_| async move { current_settings().await });
    // The link a call hands back, and the word for a call that was refused.
    // They live here, above the Transition, because a successful call refetches
    // the snapshot: signals owned by the members panel would be dropped with it
    // and the link — which the store keeps only as a hash — would be gone for
    // good before anyone could read it.
    let link = RwSignal::new(None::<String>);
    let link_refusal = RwSignal::new(None::<String>);

    view! {
        <Transition fallback=|| view! { <main class="settings-stage"></main> }>
            {move || Suspend::new(async move {
                match settings.await {
                    Ok(Ok(snapshot)) => {
                        view! {
                            <SettingsScreen
                                snapshot=snapshot
                                on_change=Callback::new(move |()| settings.refetch())
                                link=link
                                link_refusal=link_refusal
                            />
                        }
                            .into_any()
                    }
                    Ok(Err(refusal)) => {
                        view! {
                            <main class="scaffold-note">
                                <p>{refusal.message()}</p>
                                <p>
                                    <a href="/">"Back to the board"</a>
                                </p>
                            </main>
                        }
                            .into_any()
                    }
                    Err(_) => {
                        view! {
                            <main class="scaffold-note">
                                <p>"Something went wrong."</p>
                            </main>
                        }
                            .into_any()
                    }
                }
            })}
        </Transition>
    }
}

#[component]
fn SettingsScreen(
    snapshot: SettingsSnapshot,
    on_change: Callback<()>,
    link: RwSignal<Option<String>>,
    link_refusal: RwSignal<Option<String>>,
) -> impl IntoView {
    let me = snapshot.me.clone();

    view! {
        <header class="topbar">
            <div class="wordmark">
                <span class="wordmark-text">"izlek"</span>
                <span class="wordmark-dot"></span>
            </div>
            <div class="topbar-divider"></div>
            <span class="board-name">"Settings"</span>
            <div class="spacer"></div>
            <span class="topbar-who" title=me.email.clone()>
                {me.display_name.clone()}
            </span>
        </header>

        <div class="settings-shell">
            <nav class="sidenav">
                <a class="sidenav-item" href="/">
                    "Board"
                </a>
                <a class="sidenav-item" href="/rules">
                    "Mail rules"
                </a>
                <a class="sidenav-item sidenav-item-on" href="/settings">
                    "Settings"
                </a>
            </nav>

            <main class="settings-stage">
                <div class="settings-head">
                    <h1 class="settings-title">"Settings"</h1>
                    <span class="chip chip-role">{me.role.as_str().to_string()}</span>
                </div>

                <ProfilePanel me=me.clone()/>
                {snapshot
                    .sender
                    .map(|sender| view! { <SenderPanel sender=sender on_change=on_change/> })}
                {snapshot.limits.map(|limits| view! { <LimitsPanel limits=limits/> })}
                {snapshot
                    .members
                    .map(|members| {
                        view! {
                            <MembersPanel
                                members=members
                                on_change=on_change
                                link=link
                                refusal=link_refusal
                            />
                        }
                    })}
            </main>
        </div>
    }
}

#[component]
fn ProfilePanel(me: Me) -> impl IntoView {
    let action = ServerAction::<SaveProfile>::new();
    let out = ServerAction::<crate::auth::SignOut>::new();
    let value = action.value();
    let saved = move || matches!(value.get(), Some(Ok(None)));
    let refusal = move || match value.get() {
        Some(Ok(Some(refusal))) => Some(refusal.message()),
        Some(Err(_)) => Some(Refusal::Unavailable.message()),
        _ => None,
    };

    view! {
        <section class="panel">
            <div class="panel-head">
                <h2 class="panel-title">"Your profile"</h2>
            </div>
            <ActionForm action=action attr:class="panel-body">
                <label class="field">
                    <span class="field-label">"DISPLAY NAME"</span>
                    <input
                        class="field-input"
                        type="text"
                        name="display_name"
                        value=me.display_name.clone()
                        maxlength="80"
                        required
                    />
                </label>
                <label class="field">
                    <span class="field-label">"EMAIL"</span>
                    <input class="field-input" type="email" value=me.email.clone() disabled/>
                </label>
                <div class="panel-foot">
                    {move || {
                        refusal()
                            .map(|message| view! { <span class="field-error">{message}</span> })
                    }}
                    {move || {
                        saved().then(|| view! { <span class="field-note">"Saved."</span> })
                    }}
                    <button class="primary" type="submit">
                        "Save"
                    </button>
                </div>
            </ActionForm>
            <div class="panel-body panel-foot panel-foot-split">
                <ActionForm action=out>
                    <button class="quiet" type="submit">
                        "Sign out"
                    </button>
                </ActionForm>
            </div>
        </section>
    }
}

/// The sender the whole workspace mails through, as an admin edits it.
///
/// The password field is write-only in both directions: it is never filled in,
/// because the server does not send it, and leaving it empty on a save means
/// "keep the one you have". The row underneath says which of those is true, so
/// an empty box is never ambiguous.
#[component]
fn SenderPanel(sender: Sender, on_change: Callback<()>) -> impl IntoView {
    let action = ServerAction::<SaveSender>::new();
    let value = action.value();
    let saved = move || matches!(value.get(), Some(Ok(None)));
    let refusal = move || match value.get() {
        Some(Ok(Some(refusal))) => Some(refusal.message()),
        Some(Err(_)) => Some(Refusal::Unavailable.message()),
        _ => None,
    };
    // A saved sender changes the chip, the password row and whether mail moves
    // at all, so the snapshot behind this panel is refetched rather than left
    // saying what was true before the save.
    Effect::new(move |_| {
        if matches!(value.get(), Some(Ok(None))) {
            on_change.run(());
        }
    });

    // The test writes down what happened, so the panel is refetched after it
    // for the same reason it is after a save: what is on screen should be the
    // stored answer, not this browser's memory of one.
    let test = ServerAction::<SendTestMail>::new();
    let tested = test.value();
    Effect::new(move |_| {
        if matches!(tested.get(), Some(Ok(None))) {
            on_change.run(());
        }
    });
    let test_refusal = move || match tested.get() {
        Some(Ok(Some(refusal))) => Some(refusal.message()),
        Some(Err(_)) => Some(Refusal::Unavailable.message()),
        _ => None,
    };

    let connected = sender.is_connected();
    let password_set = sender.password_set;
    // What the row says: a refusal from the call itself if there was one, and
    // otherwise the stored result of the last test — which outlives the reload,
    // because "did this ever work" is a question asked the next morning.
    let last = sender.test.clone();
    let test_line = move || match (test_refusal(), last.clone()) {
        (Some(message), _) => {
            view! { <span class="field-error">{message}</span> }.into_any()
        }
        (None, Some(result)) => match (result.error, result.took) {
            (Some(problem), _) => {
                view! {
                    <span class="field-error">
                        {format!("not delivered, {} — {problem}", result.moment)}
                    </span>
                }
                    .into_any()
            }
            (None, Some(took)) => {
                view! {
                    <span class="field-note">
                        {format!("delivered in {took} — {}", result.moment)}
                    </span>
                }
                    .into_any()
            }
            (None, None) => ().into_any(),
        },
        (None, None) => ().into_any(),
    };

    view! {
        <section class="panel">
            <div class="panel-head">
                <h2 class="panel-title">"Outgoing mail"</h2>
                <span class="chip chip-admin">"Admin only"</span>
                {match connected {
                    true => view! { <span class="chip chip-connected">"Connected"</span> }.into_any(),
                    false => view! { <span class="chip chip-off">"Not configured"</span> }.into_any(),
                }}
            </div>
            <div class="panel-body">
                {(!connected)
                    .then(|| {
                        view! {
                            <p class="panel-lede">
                                "Not connected — mail queues until you save."
                            </p>
                        }
                    })}
                <ActionForm action=action attr:id="sender-settings">
                    <div class="field-row">
                        <label class="field">
                            <span class="field-label">"SMTP HOST"</span>
                            <input
                                class="field-input"
                                type="text"
                                name="host"
                                value=sender.host
                                placeholder="smtp.fastmail.com"
                            />
                        </label>
                        <label class="field field-narrow">
                            <span class="field-label">"PORT"</span>
                            <input
                                class="field-input"
                                type="number"
                                name="port"
                                min="1"
                                max="65535"
                                value=sender.port.to_string()
                            />
                        </label>
                    </div>
                    <label class="field">
                        <span class="field-label">"USERNAME"</span>
                        <input class="field-input" type="text" name="username" value=sender.username/>
                    </label>
                    <label class="field">
                        <span class="field-label">"PASSWORD"</span>
                        <input
                            class="field-input"
                            type="password"
                            name="password"
                            autocomplete="off"
                            placeholder=match password_set {
                                true => "Set — type a new one to replace it",
                                false => "Needed before anything can be sent",
                            }
                        />
                    </label>
                    <div class="field-row">
                        <label class="field">
                            <span class="field-label">"FROM NAME"</span>
                            <input
                                class="field-input"
                                type="text"
                                name="from_name"
                                value=sender.from_name
                                placeholder="Izlek"
                            />
                        </label>
                        <label class="field">
                            <span class="field-label">"FROM ADDRESS"</span>
                            <input
                                class="field-input"
                                type="text"
                                name="from_address"
                                value=sender.from_address
                                placeholder="board@izlek.sh"
                            />
                        </label>
                    </div>
                </ActionForm>
                // The test lives outside the settings form because it is its
                // own call, and a form cannot hold another. Save reaches back
                // into that form by name, which is what the `form` attribute is
                // for, and works in a browser with no script as well as one
                // with.
                <div class="panel-foot panel-foot-split">
                    <div class="foot-side">
                        <ActionForm action=test>
                            <button
                                class="quiet"
                                type="submit"
                                disabled=move || test.pending().get()
                            >
                                "Send test mail to myself"
                            </button>
                        </ActionForm>
                        {test_line}
                    </div>
                    <div class="foot-side">
                        <Show when=move || refusal().is_some()>
                            <span class="field-error">{move || refusal()}</span>
                        </Show>
                        <Show when=saved>
                            <span class="field-note">"Saved."</span>
                        </Show>
                        <button class="primary" type="submit" form="sender-settings">
                            "Save"
                        </button>
                    </div>
                </div>
            </div>
        </section>
    }
}

/// The workspace's limits. These are workspace content, not configuration: an
/// admin changes them here and nothing restarts.
#[component]
fn LimitsPanel(limits: Limits) -> impl IntoView {
    let action = ServerAction::<SaveLimits>::new();
    let value = action.value();
    let saved = move || matches!(value.get(), Some(Ok(None)));
    let refusal = move || match value.get() {
        Some(Ok(Some(refusal))) => Some(refusal.message()),
        Some(Err(_)) => Some(Refusal::Unavailable.message()),
        _ => None,
    };

    view! {
        <section class="panel">
            <div class="panel-head">
                <h2 class="panel-title">"Workspace limits"</h2>
                <span class="chip chip-admin">"Admin only"</span>
            </div>
            <ActionForm action=action attr:class="panel-body">
                <div class="field-row">
                    <label class="field">
                        <span class="field-label">"ATTACHMENT LIMIT (MB)"</span>
                        <input
                            class="field-input"
                            type="number"
                            name="attachment_limit_mb"
                            min="1"
                            max=WIDEST_ATTACHMENT_MB.to_string()
                            value=limits.attachment_limit_mb.to_string()
                            required
                        />
                    </label>
                    <label class="field">
                        <span class="field-label">"PHOTO LIMIT (MB)"</span>
                        <input
                            class="field-input"
                            type="number"
                            name="photo_limit_mb"
                            min="1"
                            max=WIDEST_PHOTO_MB.to_string()
                            value=limits.photo_limit_mb.to_string()
                            required
                        />
                    </label>
                </div>
                <label class="field">
                    <span class="field-label">"ALLOWED FILE TYPES"</span>
                    <input
                        class="field-input"
                        type="text"
                        name="allowed_file_types"
                        value=limits.allowed_file_types.join(", ")
                        placeholder="png, jpg, pdf, zip"
                    />
                </label>
                <p class="panel-lede">
                    "A lower limit never touches files already uploaded."
                </p>
                <div class="panel-foot">
                    {move || {
                        refusal()
                            .map(|message| view! { <span class="field-error">{message}</span> })
                    }}
                    {move || saved().then(|| view! { <span class="field-note">"Saved."</span> })}
                    <button class="primary" type="submit">
                        "Save"
                    </button>
                </div>
            </ActionForm>
        </section>
    }
}

#[cfg(test)]
mod file_type_tests {
    use super::parse_types;

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

    // The list is checked against a filename later, so anything that could act
    // as a path or a pattern is refused rather than stored.
    #[test]
    fn nothing_that_is_not_an_extension_is_stored_as_one() {
        assert!(parse_types("image/png").is_none());
        assert!(parse_types("../etc").is_none());
        assert!(parse_types("*").is_none());
        assert!(parse_types("tar.gz").is_none());
    }
}

/// The member list, the invitation form, and the sentence about the roles that
/// the artboard puts under it.
#[component]
fn MembersPanel(
    members: Vec<Member>,
    on_change: Callback<()>,
    /// The link a call handed back, kept only until the next call. It is shown
    /// once because it exists once: what the store holds is its hash. Owned by
    /// the page, not by this panel, so a refetch does not take it away.
    link: RwSignal<Option<String>>,
    refusal: RwSignal<Option<String>>,
) -> impl IntoView {
    let invite = ServerAction::<crate::auth::InviteMember>::new();
    let resend = ServerAction::<ResendLink>::new();

    let carry = move |value: Option<Result<Result<String, Refusal>, ServerFnError>>| {
        match value {
            Some(Ok(Ok(path))) => {
                link.set(Some(path));
                refusal.set(None);
                on_change.run(());
            }
            Some(Ok(Err(problem))) => {
                link.set(None);
                refusal.set(Some(problem.message()));
            }
            Some(Err(_)) => {
                link.set(None);
                refusal.set(Some(Refusal::Unavailable.message()));
            }
            None => {}
        }
    };
    let invited = invite.value();
    Effect::new(move |_| carry(invited.get()));
    let resent = resend.value();
    Effect::new(move |_| carry(resent.get()));

    // An invite that went through leaves its fields filled otherwise, and the
    // next click on Add member would try to create the same person again.
    let invite_form: NodeRef<leptos::html::Form> = NodeRef::new();
    Effect::new(move |_| {
        if matches!(invited.get(), Some(Ok(Ok(_)))) &&
            let Some(form) = invite_form.get()
        {
            form.reset();
        }
    });

    let rows = members
        .into_iter()
        .map(|member| {
            let account = if member.is_owner {
                "the first account, administers the workspace".to_string()
            } else if !member.has_password {
                "no password yet".to_string()
            } else if let Some(day) = member.last_signed_in.clone() {
                format!("password set — last signed in {day}")
            } else {
                "password set".to_string()
            };
            let id = member.id.clone();
            view! {
                <tr class="member-row">
                    <td class="member-name">
                        {member.display_name.clone()}
                        {member
                            .is_you
                            .then(|| view! { <span class="member-you">"you"</span> })}
                    </td>
                    <td class="member-address">{member.email.clone()}</td>
                    <td>
                        <span class="chip chip-role">{member.role.as_str().to_string()}</span>
                    </td>
                    <td class="member-account">
                        {account}
                        {(!member.has_password)
                            .then(|| {
                                view! {
                                    <ActionForm action=resend attr:class="member-resend">
                                        <input type="hidden" name="user_id" value=id.clone()/>
                                        <button class="quiet" type="submit">
                                            "Resend link"
                                        </button>
                                    </ActionForm>
                                }
                            })}
                    </td>
                </tr>
            }
        })
        .collect_view();

    view! {
        <section class="panel">
            <div class="panel-head">
                <h2 class="panel-title">"Members"</h2>
                <span class="chip chip-admin">"Admin only"</span>
            </div>
            <div class="panel-body">
                <table class="member-table">
                    <thead>
                        <tr>
                            <th>"NAME"</th>
                            <th>"ADDRESS"</th>
                            <th>"ROLE"</th>
                            <th>"ACCOUNT"</th>
                        </tr>
                    </thead>
                    <tbody>{rows}</tbody>
                </table>

                <ActionForm action=invite node_ref=invite_form attr:class="member-invite">
                    <label class="field">
                        <span class="field-label">"NAME"</span>
                        <input
                            class="field-input"
                            type="text"
                            name="display_name"
                            maxlength="80"
                            required
                        />
                    </label>
                    <label class="field">
                        <span class="field-label">"ADDRESS"</span>
                        <input class="field-input" type="email" name="email" required/>
                    </label>
                    <label class="field field-role">
                        <span class="field-label">"ROLE"</span>
                        <select class="field-input" name="role">
                            <option value="member">"Member"</option>
                            <option value="viewer">"Viewer"</option>
                            <option value="admin">"Admin"</option>
                        </select>
                    </label>
                    <button class="primary" type="submit">
                        "Add member"
                    </button>
                </ActionForm>

                {move || {
                    refusal
                        .get()
                        .map(|message| view! { <p class="field-error">{message}</p> })
                }}
                {move || {
                    link.get()
                        .map(|path| {
                            view! {
                                <div class="member-link">
                                    <span class="field-label">"SIGN-IN LINK"</span>
                                    <code class="member-link-value">{path}</code>
                                    <span class="field-note">
                                        "Shown once. Expires in 7 days."
                                    </span>
                                </div>
                            }
                        })
                }}

                <div class="role-note">
                    <p class="panel-lede">
                        <b>"Admin"</b>
                        " — sender, limits, members."
                    </p>
                    <p class="panel-lede">
                        <b>"Member"</b>
                        " — board and rule mail."
                    </p>
                    <p class="panel-lede">
                        <b>"Viewer"</b>
                        " — read and export only."
                    </p>
                </div>
            </div>
        </section>
    }
}

#[cfg(all(test, feature = "ssr"))]
mod tests {
    use super::took_label;

    #[test]
    fn a_send_is_timed_the_way_the_artboard_writes_it() {
        // The artboard's own line reads `delivered in 1.2 s`.
        assert_eq!(took_label(1234), "1.2 s");
        assert_eq!(took_label(1000), "1.0 s");
        // Under a second the seconds reading would be all zeroes, so say milliseconds.
        assert_eq!(took_label(999), "999 ms");
        assert_eq!(took_label(0), "0 ms");
    }
}
