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

/// One row of the member list, as an admin may see it. There is no password
/// and no token here: whether a password exists is a fact about the account,
/// and the token of a live link is shown once at the moment it is minted and
/// is never readable afterwards — the store keeps only its hash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub id: String,
    pub display_name: String,
    pub email: String,
    pub role: dizey_core::Role,
    pub has_password: bool,
    /// The day they last signed in, as the list writes it, or nothing.
    pub last_signed_in: Option<String>,
    pub is_you: bool,
    /// The first account. It administers the workspace and cannot be removed.
    pub is_owner: bool,
}

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
        sender: administers
            .then(|| use_context::<Option<Sender>>().flatten())
            .flatten(),
        limits,
        members,
    }))
}

#[cfg(feature = "ssr")]
async fn members_now(asking: &dizey_core::store::User) -> Result<Vec<Member>, ServerFnError> {
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
            last_signed_in: user.last_signed_in_at.map(|at| dizey_core::board::day_label(at.date())),
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
    // They live here, above the Suspense, because a successful call refetches
    // the snapshot: signals owned by the members panel would be dropped with it
    // and the link — which the store keeps only as a hash — would be gone for
    // good before anyone could read it.
    let link = RwSignal::new(None::<String>);
    let link_refusal = RwSignal::new(None::<String>);

    view! {
        <Suspense fallback=|| view! { <main class="settings-stage"></main> }>
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
fn SettingsScreen(
    snapshot: SettingsSnapshot,
    on_change: Callback<()>,
    link: RwSignal<Option<String>>,
    link_refusal: RwSignal<Option<String>>,
) -> impl IntoView {
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
        if matches!(invited.get(), Some(Ok(Ok(_)))) {
            if let Some(form) = invite_form.get() {
                form.reset();
            }
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

                <p class="panel-lede">
                    "You create the account with a name and an address. No password is set — the person picks one the first time they sign in."
                </p>

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
                                    <span class="field-label">"SIGN-IN LINK — SHOWN ONCE"</span>
                                    <code class="member-link-value">{path}</code>
                                    <span class="field-note">
                                        "Pass it on now. Dizey keeps only its hash, so nothing can show it again — send another link instead. Links expire after 7 days, and an expired link is not a dead account: resending opens the same one."
                                    </span>
                                </div>
                            }
                        })
                }}

                <div class="role-note">
                    <p class="panel-lede">
                        <b>"Admin"</b>
                        " holds the sender, the limits and this list."
                    </p>
                    <p class="panel-lede">
                        <b>"Member"</b>
                        " works the board and is mailed by the rules."
                    </p>
                    <p class="panel-lede">
                        <b>"Viewer"</b>
                        " reads and exports — cannot be assigned a task, cannot comment, and no rule ever mails them."
                    </p>
                </div>
            </div>
        </section>
    }
}
