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

use izlek_core::detail::ActivityKind;
use izlek_core::store::{ActivityFilter, Dir, FeedPage};

use crate::i18n::{Key, Lang, t};
use crate::server::{accounts, require_user};

path_param!(user_id);

/// The account line the members table renders, read the same way here: a
/// named state (owner, invited) while there is nothing newer to say, and the
/// last-seen day once there is.
fn account_line(lang: Lang, is_owner: bool, has_signed_in: bool, last_day: Option<String>) -> String {
    if is_owner {
        t(lang, Key::OwnerStatus).to_string()
    } else if !has_signed_in {
        t(lang, Key::InvitedStatus).to_string()
    } else if let Some(day) = last_day {
        crate::i18n::last_seen_label(lang, &day)
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
        ActivityKind::TestMailSent => t(lang, Key::ActTestMailSent).into(),
        ActivityKind::MessageSent => t(lang, Key::ActMessageSent).into(),
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

    let zone = izlek_core::detail::parse_zone(&user.timezone);
    let mine = person.id == user.id;
    let who = izlek_core::board::Person {
        id: person.id.clone(),
        display_name: person.display_name.clone(),
        has_photo: person.has_photo,
    };
    let is_owner = store
        .owner()
        .await?
        .is_some_and(|owner| owner.id == person.id);
    let account = account_line(
        lang,
        is_owner,
        person.has_signed_in(),
        person
            .last_signed_in_at
            .map(|at| izlek_core::board::day_label(at.to_offset(zone).date())),
    );
    let role_key = match person.role {
        izlek_core::Role::Admin => Key::RoleAdminOption,
        izlek_core::Role::Member => Key::RoleMemberOption,
        izlek_core::Role::Viewer => Key::RoleViewerOption,
    };
    let stats = store.user_stats(&person.id).await?;
    // A short list, not the feed: the newest few lines are enough to see what
    // the person has been near lately; the whole trail already has a page.
    let feed = store
        .recent_activity(
            8,
            FeedPage::Newest,
            Dir::Newest,
            &ActivityFilter { actor: Some(person.id.clone()), ..Default::default() },
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
            <section class="panel" id="profile">
                <div class="panel-head">
                    (crate::layout::avatar(cx, &who, "avatar-lg").await?)
                    <h2 class="panel-title">(person.display_name.clone())</h2>
                    <span class="chip chip-role">(t(lang, role_key))</span>
                    if mine {
                        <a class="quiet" href="/settings?section=profile">(t(lang, Key::EditLabel))</a>
                    }
                </div>
                <div class="panel-body">
                    <p>(person.email.clone())</p>
                    <p>(account)</p>
                </div>
            </section>

            <section class="panel">
                <div class="panel-body">
                    <dl class="people-stats">
                        <div><dt>(t(lang, Key::TasksOpenLabel))</dt><dd>(stats.assigned_open)</dd></div>
                        <div><dt>(t(lang, Key::TasksDoneLabel))</dt><dd>(stats.assigned_done)</dd></div>
                        <div><dt>(t(lang, Key::TasksCreatedLabel))</dt><dd>(stats.created)</dd></div>
                        <div><dt>(t(lang, Key::CommentsLabel))</dt><dd>(stats.comments)</dd></div>
                    </dl>
                </div>
            </section>

            <section class="panel">
                <div class="panel-head">
                    <h2 class="panel-title">(t(lang, Key::RecentActivity))</h2>
                </div>
                <div class="panel-body">
                    if feed.is_empty() {
                        <p>(t(lang, Key::NothingYet))</p>
                    }
                    <ul class="people-feed">
                        for line in feed {
                            <li>
                                <span>(kind_word(lang, &line.kind))</span>
                                if let Some(title) = line.title {
                                    <span>(title)</span>
                                }
                                <span class="people-feed-day">(izlek_core::board::day_label(line.at.to_offset(zone).date()))</span>
                            </li>
                        }
                    </ul>
                </div>
            </section>
        </main>

        (crate::layout::escape_script(cx).await?)
    }
}
