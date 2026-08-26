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

/// The sender the process was started with, as a screen may see it.
///
/// The password is not a field here and there is no field it could be put in.
/// It lives in `DIZEY_SMTP_PASSWORD`, is read once at boot and is held only by
/// the mailer; the database has no column for it since migration 0006. A
/// sender is all-or-nothing in configuration — five variables or none — so a
/// sender that exists is a sender whose password is set, and that is the whole
/// of what this type can say about it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sender {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub from: String,
}

#[cfg(feature = "ssr")]
impl Sender {
    /// What the screen may see of the sender the process is running with.
    pub fn of(config: &dizey_core::config::MailConfig) -> Self {
        Self {
            host: config.host.clone(),
            port: config.port,
            username: config.username.clone(),
            from: config.from.clone(),
        }
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

const MB: u64 = 1024 * 1024;

/// One settings screen's worth of state. `sender` is `None` for two different
/// reasons — no sender configured, or not an admin asking — and neither of
/// them is a reason to send a host name to somebody who may not have it, so
/// the two are one answer here.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SettingsSnapshot {
    pub me: Me,
    pub administers: bool,
    pub sender: Option<Sender>,
    /// The limits, for the admin who may change them. A Member's answer does
    /// not carry the panel's contents.
    pub limits: Option<Limits>,
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
    Ok(Ok(SettingsSnapshot {
        me: Me {
            id: user.id,
            display_name: user.display_name,
            email: user.email,
            role: user.role,
        },
        administers,
        // Only an admin is told anything about the sender at all.
        sender: administers
            .then(|| use_context::<Option<Sender>>().flatten())
            .flatten(),
        limits: match administers {
            true => Some(limits_now(&user.workspace_id).await?),
            false => None,
        },
    }))
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

    view! {
        <Suspense fallback=|| view! { <main class="settings-stage"></main> }>
            {move || Suspend::new(async move {
                match settings.await {
                    Ok(Ok(snapshot)) => view! { <SettingsScreen snapshot=snapshot/> }.into_any(),
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
                                <p>"Something went wrong. Reload the page."</p>
                            </main>
                        }
                            .into_any()
                    }
                }
            })}
        </Suspense>
    }
}

#[component]
fn SettingsScreen(snapshot: SettingsSnapshot) -> impl IntoView {
    let me = snapshot.me.clone();
    let has_sender = snapshot.sender.is_some();
    let role_note = if snapshot.administers {
        "First account in this workspace. Only you see the sender, limits and member panels."
    } else {
        "You work the board and the rules mail you. The sender, the limits and the member list are the admin's."
    };

    view! {
        <header class="topbar">
            <div class="wordmark">
                <span class="wordmark-text">"dizey"</span>
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
                <a class="sidenav-item sidenav-item-on" href="/settings">
                    "Settings"
                </a>
            </nav>

            <main class="settings-stage">
                <div class="settings-head">
                    <h1 class="settings-title">"Settings"</h1>
                    <span class="chip chip-role">{me.role.as_str().to_string()}</span>
                    <span class="settings-note">{role_note}</span>
                </div>

                <ProfilePanel me=me.clone()/>
                {snapshot
                    .sender
                    .map(|sender| view! { <SenderPanel sender=sender/> })}
                {(snapshot.administers && !has_sender)
                    .then(|| view! { <NoSenderPanel/> })}
                {snapshot.limits.map(|limits| view! { <LimitsPanel limits=limits/> })}
            </main>
        </div>
    }
}

#[component]
fn ProfilePanel(me: Me) -> impl IntoView {
    let action = ServerAction::<SaveProfile>::new();
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
                    <span class="field-label">"EMAIL — WHERE YOUR NOTIFICATIONS GO"</span>
                    <input class="field-input" type="email" value=me.email.clone() disabled/>
                    <span class="field-note">
                        "Your address is the account. The admin changes it from the member list."
                    </span>
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
        </section>
    }
}

/// The sender, as the process is running it. Every control is disabled and
/// says why: none of this is workspace content, so there is nothing here a
/// save could write.
#[component]
fn SenderPanel(sender: Sender) -> impl IntoView {
    view! {
        <section class="panel">
            <div class="panel-head">
                <h2 class="panel-title">"Outgoing mail"</h2>
                <span class="chip chip-admin">"Admin only"</span>
                <span class="chip chip-connected">"Connected"</span>
            </div>
            <div class="panel-body">
                <p class="panel-lede">
                    "One sender for the whole workspace. Every mail rule, for every member, sends through this account — nobody sets their own."
                </p>
                <div class="field-row">
                    <label class="field">
                        <span class="field-label">"SMTP HOST"</span>
                        <input class="field-input" type="text" value=sender.host disabled/>
                    </label>
                    <label class="field field-narrow">
                        <span class="field-label">"PORT"</span>
                        <input
                            class="field-input"
                            type="text"
                            value=sender.port.to_string()
                            disabled
                        />
                    </label>
                </div>
                <label class="field">
                    <span class="field-label">"USERNAME"</span>
                    <input class="field-input" type="text" value=sender.username disabled/>
                </label>
                <label class="field">
                    <span class="field-label">"PASSWORD"</span>
                    <div class="field-stated">"Set — in the server's configuration"</div>
                    <span class="field-note">
                        "The password is read from DIZEY_SMTP_PASSWORD when the process starts and is never stored, never shown and never sent to this page. Changing it means changing that variable and restarting Dizey."
                    </span>
                </label>
                <label class="field">
                    <span class="field-label">"FROM ADDRESS"</span>
                    <input class="field-input" type="text" value=sender.from disabled/>
                </label>
                <p class="panel-lede">
                    "This whole panel is what the running process was configured with. It is read here and changed where it is set — DIZEY_SMTP_HOST, DIZEY_SMTP_PORT, DIZEY_SMTP_USERNAME, DIZEY_SMTP_PASSWORD and DIZEY_MAIL_FROM — followed by a restart."
                </p>
            </div>
        </section>
    }
}

/// An admin on a workspace with no sender. Nothing is broken; nothing is sent.
#[component]
fn NoSenderPanel() -> impl IntoView {
    view! {
        <section class="panel">
            <div class="panel-head">
                <h2 class="panel-title">"Outgoing mail"</h2>
                <span class="chip chip-admin">"Admin only"</span>
                <span class="chip chip-off">"Not configured"</span>
            </div>
            <div class="panel-body">
                <p class="panel-lede">
                    "No sender is configured, so Dizey sends nothing. Cards still move and rules can still be written — what they owe is kept and goes out once a sender exists."
                </p>
                <p class="panel-lede">
                    "Set DIZEY_SMTP_HOST, DIZEY_SMTP_PORT, DIZEY_SMTP_USERNAME, DIZEY_SMTP_PASSWORD and DIZEY_MAIL_FROM — all five or none — and restart Dizey."
                </p>
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
                        <span class="field-label">"ATTACHMENT SIZE LIMIT"</span>
                        <input
                            class="field-input"
                            type="number"
                            name="attachment_limit_mb"
                            min="1"
                            max=WIDEST_ATTACHMENT_MB.to_string()
                            value=limits.attachment_limit_mb.to_string()
                            required
                        />
                        <span class="field-note">
                            "Megabytes per file, for task attachments and comment files. The limit is enforced when the file arrives, not only in the picker."
                        </span>
                    </label>
                    <label class="field">
                        <span class="field-label">"PROFILE PHOTO LIMIT"</span>
                        <input
                            class="field-input"
                            type="number"
                            name="photo_limit_mb"
                            min="1"
                            max=WIDEST_PHOTO_MB.to_string()
                            value=limits.photo_limit_mb.to_string()
                            required
                        />
                        <span class="field-note">"Megabytes per photo."</span>
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
                    <span class="field-note">
                        "Extensions, separated by commas. An empty list means every type is allowed."
                    </span>
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
