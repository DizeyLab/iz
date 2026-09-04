//! A member's profile: who they are, and what they have done here. Any
//! signed-in member of the workspace may read it, and nothing on the page
//! writes — the one editor a profile has stays where it already lives, on
//! `/settings?section=profile`, which the page links to for its own person
//! and nowhere else. A stranger and a missing id read the same: the router's
//! own not-found, never a `403` — `photo.rs` keeps the same rule for bytes.

use topcoat::Result;
use topcoat::context::Cx;
use topcoat::router::error::not_found;
use topcoat::router::{page, path_param};
use topcoat::view::view;

use iz_core::detail::ActivityKind;
use iz_core::store::{ActivityFilter, Dir, FeedPage};

use crate::i18n::{Key, Lang, t};
use crate::server::{accounts, require_user};

path_param!(user_id);

/// What the account itself is doing: the owner reads as the owner, somebody
/// who has never chosen a password reads as invited, and everybody else is
/// the day they were last here.
fn last_seen(lang: Lang, has_signed_in: bool, last_day: Option<String>) -> String {
    if !has_signed_in {
        t(lang, Key::InvitedStatus).to_string()
    } else if let Some(day) = last_day {
        day
    } else {
        t(lang, Key::ActiveStatus).to_string()
    }
}

/// The verb a feed line opens with. The task kinds read through
/// [`crate::i18n::activity_kind_word`]; the account kinds are not all in that
/// table and would fall through as raw tokens (`profile_saved`), so the ones
/// with a translated word take it here.
fn kind_word(lang: Lang, kind: &ActivityKind) -> String {
    match kind {
        ActivityKind::WorkspaceClaimed => t(lang, Key::ActWorkspaceClaimed).into(),
        ActivityKind::Joined => t(lang, Key::ActJoined).into(),
        ActivityKind::SignedIn => t(lang, Key::ActSignedIn).into(),
        ActivityKind::SignedOut => t(lang, Key::ActSignedOut).into(),
        ActivityKind::PasswordChanged => t(lang, Key::ActPasswordChanged).into(),
        ActivityKind::ProfileSaved => t(lang, Key::ActProfileSaved).into(),
        ActivityKind::SenderSaved => t(lang, Key::ActSenderSaved).into(),
        ActivityKind::LimitsSaved => t(lang, Key::ActLimitsSaved).into(),
        ActivityKind::SecuritySaved => t(lang, Key::ActSecuritySaved).into(),
        ActivityKind::TestMailSent => t(lang, Key::ActTestMailSent).into(),
        ActivityKind::MessageSent => t(lang, Key::ActMessageSent).into(),
        ActivityKind::Other(raw) if raw == "password_reset_requested" => {
            t(lang, Key::ActResetRequested).into()
        }
        ActivityKind::Other(raw) if raw == "password_reset_completed" => {
            t(lang, Key::ActResetDone).into()
        }
        other => crate::i18n::activity_kind_word(lang, other.as_str()),
    }
}

/// A person's page. `/people/{user_id}`.
#[page("/people/{user_id}")]
async fn people_page(cx: &Cx) -> Result {
    let user = match require_user(cx).await {
        Ok(user) => user,
        Err(_) => return Err(not_found().into()),
    };
    let lang = Lang::from_code(&user.language);
    let store = accounts(cx).store().clone();
    let target_id: &str = path_param::<UserId>(cx);
    let Some(person) = store.user(target_id).await? else {
        return Err(not_found().into());
    };
    if person.workspace_id != user.workspace_id {
        return Err(not_found().into());
    }

    let zone = iz_core::detail::parse_zone(&user.timezone);
    let mine = person.id == user.id;
    let who = iz_core::board::Person {
        id: person.id.clone(),
        display_name: person.display_name.clone(),
        has_photo: person.has_photo,
    };
    let is_owner = store
        .owner()
        .await?
        .is_some_and(|owner| owner.id == person.id);
    let joined = iz_core::board::day_label(person.created_at.to_offset(zone).date());
    let seen = last_seen(
        lang,
        person.has_signed_in(),
        person
            .last_signed_in_at
            .map(|at| iz_core::board::day_label(at.to_offset(zone).date())),
    );
    // Who let them in, as a name that leads to that person's own page. The
    // first account was invited by nobody.
    let inviter = match &person.invited_by {
        Some(id) => store.user(id).await?,
        None => None,
    };
    let role_key = match person.role {
        iz_core::Role::Admin => Key::RoleAdminOption,
        iz_core::Role::Member => Key::RoleMemberOption,
        iz_core::Role::Viewer => Key::RoleViewerOption,
    };
    let stats = store.user_stats(&person.id).await?;
    // A short list, not the feed: the newest few lines are enough to see what
    // the person has been near lately; the whole trail already has a page.
    let feed = store
        .recent_activity(
            8,
            FeedPage::Newest,
            Dir::Newest,
            &ActivityFilter {
                actor: Some(person.id.clone()),
                ..Default::default()
            },
        )
        .await?;

    view! {
        cx =>
        <header class="topbar">
            (crate::layout::mark(cx).await?)
            (crate::layout::topbar_nav(cx, crate::layout::NavPage::Settings, user.role, lang).await?)
            <div class="spacer"></div>
            (crate::layout::user_menu(cx, &crate::detail::Me::from(&user), lang).await?)
        </header>

        <main class="people-shell">
            <section class="panel person-card">
                <div class="person-head">
                    if person.has_photo {
                        <button class="avatar-view" type="button" data-close-label=(t(lang, Key::Close))>
                            (crate::layout::avatar(cx, &who, "avatar-xl").await?)
                        </button>
                    } else {
                        (crate::layout::avatar(cx, &who, "avatar-xl").await?)
                    }
                    <div class="person-heading">
                        <h2 class="person-name">(person.display_name.clone())</h2>
                        <div class="person-marks">
                            <span class="chip chip-role">(t(lang, role_key))</span>
                            if is_owner {
                                <span class="chip chip-admin">(t(lang, Key::OwnerStatus))</span>
                            }
                        </div>
                    </div>
                    if mine {
                        <a class="quiet person-edit" href="/settings?section=profile">(t(lang, Key::EditLabel))</a>
                    }
                </div>
                <dl class="person-fields">
                    <div class="person-field">
                        <dt>(t(lang, Key::EmailLabel))</dt>
                        <dd class="person-address">(person.email.clone())</dd>
                    </div>
                    <div class="person-field">
                        <dt>(t(lang, Key::JoinedLabel))</dt>
                        <dd>(joined)</dd>
                    </div>
                    <div class="person-field">
                        <dt>(t(lang, Key::LastSeenLabel))</dt>
                        <dd>(seen)</dd>
                    </div>
                    if let Some(inviter) = inviter {
                        <div class="person-field">
                            <dt>(t(lang, Key::InvitedByLabel))</dt>
                            <dd><a href=(format!("/people/{}", inviter.id))>(inviter.display_name.clone())</a></dd>
                        </div>
                    }
                </dl>
            </section>

            <section class="panel">
                <dl class="person-stats">
                    <div class="person-stat">
                        <dd>(stats.assigned_open)</dd>
                        <dt>(t(lang, Key::TasksOpenLabel))</dt>
                    </div>
                    <div class="person-stat">
                        <dd>(stats.assigned_done)</dd>
                        <dt>(t(lang, Key::TasksDoneLabel))</dt>
                    </div>
                    <div class="person-stat">
                        <dd>(stats.created)</dd>
                        <dt>(t(lang, Key::TasksCreatedLabel))</dt>
                    </div>
                    <div class="person-stat">
                        <dd>(stats.comments)</dd>
                        <dt>(t(lang, Key::CommentsLabel))</dt>
                    </div>
                </dl>
            </section>

            <section class="panel">
                <div class="panel-head">
                    <h2 class="panel-title">(t(lang, Key::RecentActivity))</h2>
                </div>
                if feed.is_empty() {
                    <div class="panel-body"><p class="muted">(t(lang, Key::NothingYet))</p></div>
                }
                <ul class="person-feed">
                    for line in feed {
                        <li class="person-line">
                            <span class="person-verb">(kind_word(lang, &line.kind))</span>
                            if let Some(task_id) = line.task_id.clone() {
                                <a class="person-task" href=(format!("/?task={task_id}"))>
                                    if let Some(key) = line.task_key.clone() {
                                        <span class="person-key">(key)</span>
                                    }
                                    <span class="person-title">(line.title.clone().unwrap_or_default())</span>
                                </a>
                            } else {
                                <span class="person-title">(line.title.clone().unwrap_or_default())</span>
                            }
                            <span class="person-day">(iz_core::board::day_label(line.at.to_offset(zone).date()))</span>
                        </li>
                    }
                </ul>
            </section>
        </main>

        (crate::layout::escape_script(cx).await?)
        (crate::detail::escape_closes(cx).await?)
        (crate::layout::avatar_script(cx).await?)
    }
}
