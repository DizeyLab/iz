//! Mail rules, ported from `izlek-web/src/rules.rs` onto topcoat's
//! server-rendered pages.
//!
//! The screen is the admin's, and so is every call behind it: a rule sends mail
//! on everyone's behalf, from the one workspace sender, so writing one is not
//! something a Member may do even if they reach the endpoint directly. The
//! chip in the artboard says "Admin only"; the guard is in the handlers.
//!
//! There is no client-side signal here to hold which row is being edited —
//! every topcoat page is rendered fresh, server-side, on every request. Which
//! rule (if any) is open for editing rides `?edit=<rule_id>` on `/rules`
//! itself: the row renders as `RuleForm` server-side instead of the display
//! row, no script required. The mutating endpoints preserve that query pair
//! across their redirect (see `crate::server::refusal_of`'s `carrying`), so a
//! failed edit lands back on the same open row rather than closing it.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use topcoat::Result;
use topcoat::context::Cx;
use topcoat::router::content::{Form, Json};
use topcoat::router::request::headers;
use topcoat::router::{HeaderName, StatusCode, header, page, query_params, route};
use topcoat::view::view;

use izlek_core::board::Column;
use izlek_core::store::{Audience, Store, Trigger, User, Workspace};

use crate::i18n::{Key, Lang, t};
use crate::server::{Refusal, accounts, refusal_of, require_admin};

/// One rule as the screen reads it: a sentence, a switch and a stamp.
///
/// The trigger arrives already split into the words the artboard prints —
/// "When status becomes" / "Done" — because the two halves are chosen from
/// different vocabularies and only the server knows the column names.
#[derive(Clone, Debug, Serialize)]
struct RuleLine {
    id: String,
    /// "When status becomes", or "When a task stops being".
    when: String,
    /// "Done", or "blocked", or empty when the trigger has no second half.
    what: String,
    subject: String,
    /// "assignees", "everyone on board", or "its creator".
    audience: String,
    /// The trigger's own word — "status", "unblocked", "created" — as the
    /// edit form's select expects it, not as the sentence prints it.
    trigger_kind: String,
    /// The named column, for a status trigger's edit form. `None` for every
    /// other trigger.
    column_id: Option<String>,
    /// The audience's own word — "assignees", "board", "creator" — as the
    /// edit form's select expects it.
    audience_kind: String,
    include_task_details: bool,
    enabled: bool,
    /// `Aug 24 14:02`, or nothing when no mail from this rule has ever been
    /// accepted by the mail server.
    last_sent: Option<String>,
    /// `Aug 24 14:02` of the rule's last decision, sent or not, or nothing
    /// when the rule has never been evaluated.
    last_fired: Option<String>,
}

/// A column, for the composer's status list.
#[derive(Clone, Debug, Serialize)]
struct ColumnChoice {
    id: String,
    name: String,
}

/// The screen in one answer.
#[derive(Clone, Debug, Serialize)]
struct RulesSnapshot {
    rules: Vec<RuleLine>,
    columns: Vec<ColumnChoice>,
    /// Whether a sender is connected. Rules that fire with no sender queue
    /// rather than fail, and the screen says so instead of leaving somebody to
    /// wonder why nothing arrived.
    sender_connected: bool,
}

/// The word for an audience, as the artboard writes it.
fn audience_word(audience: Audience, lang: Lang) -> &'static str {
    match audience {
        Audience::Assignees => t(lang, Key::AudAssignees),
        Audience::Board => t(lang, Key::AudEveryoneOnBoard),
        Audience::Creator => t(lang, Key::AudItsCreator),
    }
}

/// The audience's own word, as the edit form's select names it.
fn audience_kind(audience: Audience) -> &'static str {
    match audience {
        Audience::Assignees => "assignees",
        Audience::Board => "board",
        Audience::Creator => "creator",
    }
}

/// Parses a trigger from the words a form sends: the trigger's own kind, and
/// a column id only a status trigger carries. Every other trigger must arrive
/// with no column — a column id on, say, "created" is a form gone wrong, not
/// something to guess past.
///
/// Shared by `create_rule` and `update_rule` so the vocabulary is matched
/// once.
fn trigger_of(kind: &str, column_id: Option<String>) -> Option<Trigger> {
    match (kind, column_id) {
        ("status", Some(column_id)) => Some(Trigger::StatusBecomes(column_id)),
        ("status", None) => None,
        ("unblocked", None) => Some(Trigger::Unblocked),
        ("created", None) => Some(Trigger::Created),
        ("assigned", None) => Some(Trigger::Assigned),
        ("unassigned", None) => Some(Trigger::Unassigned),
        ("commented", None) => Some(Trigger::Commented),
        ("deadline_set", None) => Some(Trigger::DeadlineSet),
        ("deadline_cleared", None) => Some(Trigger::DeadlineCleared),
        ("retitled", None) => Some(Trigger::Retitled),
        ("linked", None) => Some(Trigger::Linked),
        ("unlinked", None) => Some(Trigger::Unlinked),
        ("deleted", None) => Some(Trigger::Deleted),
        _ => None,
    }
}

/// The audience a form's own word names, or the refusal for anything else —
/// shared by `create_rule` and `update_rule`.
fn audience_of(word: &str) -> std::result::Result<Audience, Refusal> {
    match word {
        "assignees" => Ok(Audience::Assignees),
        "board" => Ok(Audience::Board),
        "creator" => Ok(Audience::Creator),
        _ => Err(Refusal::Forbidden),
    }
}

/// Whether a sender is connected, read straight off the raw workspace row —
/// this module does not reach into `crate::settings::Sender` so it stays
/// independent of that screen's own shape.
fn sender_connected(workspace: &Workspace) -> bool {
    !workspace
        .smtp_host
        .as_deref()
        .unwrap_or_default()
        .trim()
        .is_empty()
        && !workspace
            .smtp_username
            .as_deref()
            .unwrap_or_default()
            .trim()
            .is_empty()
        && !workspace
            .smtp_from_address
            .as_deref()
            .unwrap_or_default()
            .trim()
            .is_empty()
        && workspace.smtp_password_set
}

/// Splits a trigger into the sentence halves the screen prints, ported
/// verbatim from `current_rules`'s match in `izlek-web/src/rules.rs`.
fn sentence_of(
    trigger: &Trigger,
    columns: &[Column],
    lang: Lang,
) -> (String, String, String, Option<String>) {
    match trigger {
        Trigger::StatusBecomes(column_id) => (
            t(lang, Key::WhenStatusBecomes).to_string(),
            columns
                .iter()
                .find(|column| &column.id == column_id)
                // A rule can outlive the column it names. Saying so is
                // better than printing an id nobody can read.
                .map(|column| column.name.clone())
                .unwrap_or_else(|| t(lang, Key::ColumnGone).to_string()),
            "status".to_string(),
            Some(column_id.clone()),
        ),
        Trigger::Unblocked => (
            t(lang, Key::WhenTaskStopsBeingBlocked).to_string(),
            t(lang, Key::BlockedWord).to_string(),
            "unblocked".to_string(),
            None,
        ),
        Trigger::Created => (
            t(lang, Key::WhenTaskCreated).to_string(),
            String::new(),
            "created".to_string(),
            None,
        ),
        Trigger::Assigned => (
            t(lang, Key::WhenSomeoneAssigned).to_string(),
            String::new(),
            "assigned".to_string(),
            None,
        ),
        Trigger::Unassigned => (
            t(lang, Key::WhenSomeoneUnassigned).to_string(),
            String::new(),
            "unassigned".to_string(),
            None,
        ),
        Trigger::Commented => (
            t(lang, Key::WhenCommentWritten).to_string(),
            String::new(),
            "commented".to_string(),
            None,
        ),
        Trigger::DeadlineSet => (
            t(lang, Key::WhenDeadlineSet).to_string(),
            String::new(),
            "deadline_set".to_string(),
            None,
        ),
        Trigger::DeadlineCleared => (
            t(lang, Key::WhenDeadlineRemoved).to_string(),
            String::new(),
            "deadline_cleared".to_string(),
            None,
        ),
        Trigger::Retitled => (
            t(lang, Key::WhenTaskRenamed).to_string(),
            String::new(),
            "retitled".to_string(),
            None,
        ),
        Trigger::Linked => (
            t(lang, Key::WhenTaskLinked).to_string(),
            String::new(),
            "linked".to_string(),
            None,
        ),
        Trigger::Unlinked => (
            t(lang, Key::WhenTaskUnlinked).to_string(),
            String::new(),
            "unlinked".to_string(),
            None,
        ),
        Trigger::Deleted => (
            t(lang, Key::WhenTaskDeleted).to_string(),
            String::new(),
            "deleted".to_string(),
            None,
        ),
    }
}

/// Every rule on the board, switched-off ones included, with the columns a new
/// rule may name — shared by the page and `current_rules`, so the two never
/// drift apart.
async fn snapshot_of(
    store: &Arc<dyn Store>,
    user: &User,
) -> std::result::Result<RulesSnapshot, Refusal> {
    let lang = Lang::from_code(&user.language);
    let zone = izlek_core::detail::parse_zone(&user.timezone);
    let Some(board) = store
        .board(&user.workspace_id)
        .await
        .map_err(|_| Refusal::Unavailable)?
    else {
        return Err(Refusal::Unavailable);
    };
    let columns = store
        .columns(&board.id)
        .await
        .map_err(|_| Refusal::Unavailable)?;
    let rules = store
        .mail_rules(&board.id)
        .await
        .map_err(|_| Refusal::Unavailable)?;
    let last_sent = store
        .mail_rule_last_sent(&board.id)
        .await
        .map_err(|_| Refusal::Unavailable)?;
    let last_decision = store
        .mail_rule_last_decision()
        .await
        .map_err(|_| Refusal::Unavailable)?;
    let sender_connected = store
        .workspace()
        .await
        .map_err(|_| Refusal::Unavailable)?
        .as_ref()
        .is_some_and(sender_connected);

    let lines = rules
        .into_iter()
        .map(|rule| {
            let (when, what, trigger_kind, column_id) = sentence_of(&rule.trigger, &columns, lang);
            RuleLine {
                when,
                what,
                subject: rule.subject.clone(),
                audience: audience_word(rule.audience, lang).to_string(),
                trigger_kind,
                column_id,
                audience_kind: audience_kind(rule.audience).to_string(),
                include_task_details: rule.include_task_details,
                enabled: rule.enabled,
                last_sent: last_sent
                    .iter()
                    .find(|(id, _)| id == &rule.id)
                    .map(|(_, at)| izlek_core::detail::moment_label_in(*at, zone)),
                last_fired: last_decision
                    .iter()
                    .find(|(id, _)| id == &rule.id)
                    .map(|(_, at)| izlek_core::detail::moment_label_in(*at, zone)),
                id: rule.id,
            }
        })
        .collect();

    Ok(RulesSnapshot {
        rules: lines,
        columns: columns
            .into_iter()
            .map(|column| ColumnChoice {
                id: column.id,
                name: column.name,
            })
            .collect(),
        sender_connected,
    })
}

/// The admin's store, once the rule id has been shown to name a rule on this
/// workspace's own board.
///
/// A rule id is opaque and arrives from the browser, so "not yours" and "not a
/// rule" are the same answer: neither tells the caller whether the id exists
/// somewhere else.
async fn rule_of_this_workspace(
    cx: &Cx,
    rule_id: &str,
) -> std::result::Result<(Arc<dyn Store>, User, izlek_core::store::MailRule), Refusal> {
    let user = require_admin(cx).await?;
    let store = accounts(cx).store().clone();
    let board = store
        .board(&user.workspace_id)
        .await
        .map_err(|_| Refusal::Unavailable)?
        .ok_or(Refusal::Unavailable)?;
    let rule = store
        .mail_rule(rule_id)
        .await
        .map_err(|_| Refusal::Unavailable)?
        .ok_or(Refusal::NotFound)?;
    if rule.board_id != board.id {
        return Err(Refusal::NotFound);
    }
    Ok((store, user, rule))
}

/// Every rule on the board, as JSON — kept for callers that post rather than
/// render, such as the http test suite.
#[route(POST "/api/current_rules")]
async fn current_rules(cx: &Cx) -> Result<Json<std::result::Result<RulesSnapshot, Refusal>>> {
    let user = match require_admin(cx).await {
        Ok(user) => user,
        Err(refusal) => return Ok(Json(Err(refusal))),
    };
    let store = accounts(cx).store().clone();
    Ok(Json(snapshot_of(&store, &user).await))
}

/// The page a browser without script is sent back to on a 303: the page the
/// form was posted from, or home when there is no `Referer` to read.
fn back_to(cx: &Cx) -> String {
    headers(cx)
        .get(header::REFERER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("/")
        .to_string()
}

/// A 303 to [`back_to`], carrying `refusal` as the body for
/// `crate::server::carry_refusal_on_redirect` to read and copy onto the query.
type Redirect = Result<(StatusCode, [(HeaderName, String); 1], Json<Option<Refusal>>)>;

fn redirect(cx: &Cx, refusal: Option<Refusal>) -> Redirect {
    Ok((
        StatusCode::SEE_OTHER,
        [(header::LOCATION, back_to(cx))],
        Json(refusal),
    ))
}

#[derive(Deserialize)]
struct CreateRuleForm {
    trigger: String,
    column_id: String,
    subject: String,
    audience: String,
    #[serde(default)]
    include_task_details: String,
}

/// Writes one rule.
///
/// `column_id` is only read for the status trigger, and is checked against
/// this workspace's own board rather than trusted — a column id from
/// somewhere else would otherwise be a way to hang a rule off another
/// workspace's board.
#[route(POST "/api/create_rule")]
async fn create_rule(cx: &Cx, Form(input): Form<CreateRuleForm>) -> Redirect {
    let user = match require_admin(cx).await {
        Ok(user) => user,
        Err(refusal) => return redirect(cx, Some(refusal)),
    };
    let subject = input.subject.trim().to_string();
    if subject.is_empty() {
        return redirect(cx, Some(Refusal::EmptySubject));
    }
    let audience = match audience_of(&input.audience) {
        Ok(audience) => audience,
        Err(refusal) => return redirect(cx, Some(refusal)),
    };

    let store = accounts(cx).store().clone();
    let board = match store.board(&user.workspace_id).await {
        Ok(Some(board)) => board,
        _ => return redirect(cx, Some(Refusal::Unavailable)),
    };
    let column_id = if input.column_id.trim().is_empty() {
        None
    } else {
        Some(input.column_id)
    };
    if input.trigger == "status" {
        let Some(column_id) = &column_id else {
            return redirect(cx, Some(Refusal::Forbidden));
        };
        let columns = match store.columns(&board.id).await {
            Ok(columns) => columns,
            Err(_) => return redirect(cx, Some(Refusal::Unavailable)),
        };
        if !columns.iter().any(|column| &column.id == column_id) {
            return redirect(cx, Some(Refusal::Forbidden));
        }
    }
    let Some(trigger) = trigger_of(&input.trigger, column_id) else {
        return redirect(cx, Some(Refusal::Forbidden));
    };

    if store
        .create_mail_rule(
            &board.id,
            &trigger,
            &subject,
            audience,
            time::OffsetDateTime::now_utc(),
            input.include_task_details == "true",
        )
        .await
        .is_err()
    {
        return redirect(cx, Some(Refusal::Unavailable));
    }
    let _ = store
        .record_event(
            Some(&user.id),
            &izlek_core::detail::ActivityKind::RuleCreated,
            &subject,
            time::OffsetDateTime::now_utc(),
        )
        .await;
    redirect(cx, None)
}

#[derive(Deserialize)]
struct UpdateRuleForm {
    rule_id: String,
    trigger: String,
    column_id: String,
    subject: String,
    audience: String,
    #[serde(default)]
    include_task_details: String,
}

/// Rewrites a rule's sentence in place. Guarded exactly like `create_rule` —
/// same subject rule, same column-belongs-to-this-board check — plus the
/// rule itself has to be this workspace's, or an id from elsewhere could be
/// rewritten by an admin who never owned it.
#[route(POST "/api/update_rule")]
async fn update_rule(cx: &Cx, Form(input): Form<UpdateRuleForm>) -> Redirect {
    let user = match require_admin(cx).await {
        Ok(user) => user,
        Err(refusal) => return redirect(cx, Some(refusal)),
    };
    let subject = input.subject.trim().to_string();
    if subject.is_empty() {
        return redirect(cx, Some(Refusal::EmptySubject));
    }
    let audience = match audience_of(&input.audience) {
        Ok(audience) => audience,
        Err(refusal) => return redirect(cx, Some(refusal)),
    };

    let store = accounts(cx).store().clone();
    let board = match store.board(&user.workspace_id).await {
        Ok(Some(board)) => board,
        _ => return redirect(cx, Some(Refusal::Unavailable)),
    };
    match store.mail_rule(&input.rule_id).await {
        Ok(Some(rule)) if rule.board_id == board.id => {}
        Ok(_) => return redirect(cx, Some(Refusal::NotFound)),
        Err(_) => return redirect(cx, Some(Refusal::Unavailable)),
    }

    let column_id = if input.column_id.trim().is_empty() {
        None
    } else {
        Some(input.column_id)
    };
    if input.trigger == "status" {
        let Some(column_id) = &column_id else {
            return redirect(cx, Some(Refusal::Forbidden));
        };
        let columns = match store.columns(&board.id).await {
            Ok(columns) => columns,
            Err(_) => return redirect(cx, Some(Refusal::Unavailable)),
        };
        if !columns.iter().any(|column| &column.id == column_id) {
            return redirect(cx, Some(Refusal::Forbidden));
        }
    }
    let Some(trigger) = trigger_of(&input.trigger, column_id) else {
        return redirect(cx, Some(Refusal::Forbidden));
    };

    if store
        .update_mail_rule(
            &input.rule_id,
            &trigger,
            &subject,
            audience,
            input.include_task_details == "true",
        )
        .await
        .is_err()
    {
        return redirect(cx, Some(Refusal::Unavailable));
    }
    let _ = store
        .record_event(
            Some(&user.id),
            &izlek_core::detail::ActivityKind::RuleEdited,
            &subject,
            time::OffsetDateTime::now_utc(),
        )
        .await;
    redirect(cx, None)
}

#[derive(Deserialize)]
struct SetRuleEnabledForm {
    rule_id: String,
    enabled: String,
}

/// Turns one rule on or off. Switching off stops it firing from here on; mail
/// it already owes has been written to the ledger and is still owed.
#[route(POST "/api/set_rule_enabled")]
async fn set_rule_enabled(cx: &Cx, Form(input): Form<SetRuleEnabledForm>) -> Redirect {
    let (store, user, rule) = match rule_of_this_workspace(cx, &input.rule_id).await {
        Ok(triple) => triple,
        Err(refusal) => return redirect(cx, Some(refusal)),
    };
    if store
        .set_mail_rule_enabled(&input.rule_id, input.enabled == "true")
        .await
        .is_err()
    {
        return redirect(cx, Some(Refusal::Unavailable));
    }
    let _ = store
        .record_event(
            Some(&user.id),
            &izlek_core::detail::ActivityKind::RuleToggled,
            &rule.subject,
            time::OffsetDateTime::now_utc(),
        )
        .await;
    redirect(cx, None)
}

#[derive(Deserialize)]
struct DeleteRuleForm {
    rule_id: String,
}

/// Removes a rule. What it has already sent stays in the ledger: the rule is
/// gone, the record of what went out is not.
#[route(POST "/api/delete_rule")]
async fn delete_rule(cx: &Cx, Form(input): Form<DeleteRuleForm>) -> Redirect {
    let (store, user, rule) = match rule_of_this_workspace(cx, &input.rule_id).await {
        Ok(triple) => triple,
        Err(refusal) => return redirect(cx, Some(refusal)),
    };
    if store.delete_mail_rule(&input.rule_id).await.is_err() {
        return redirect(cx, Some(Refusal::Unavailable));
    }
    let _ = store
        .record_event(
            Some(&user.id),
            &izlek_core::detail::ActivityKind::RuleDeleted,
            &rule.subject,
            time::OffsetDateTime::now_utc(),
        )
        .await;
    redirect(cx, None)
}

/// Which rule (if any) `/rules` renders open for editing.
#[query_params(error = redirect("/rules"))]
struct RulesQuery {
    edit: Option<String>,
}

/// The trigger/column/subject/audience form: one view for both writing a new
/// rule and rewriting an existing one, so the two never drift apart.
///
/// `existing` is `None` for a fresh rule and `Some` to edit one in place. The
/// column select only shows for the status trigger; every other trigger
/// carries `column_id` as a hidden field instead, the same trick
/// `izlek-web/src/rules.rs` used to hide it live — here it is decided once,
/// server-side, from whichever trigger the form starts on (the existing
/// rule's, or "status" for a fresh one), since there is no script to flip it
/// as the admin changes the select.
async fn rule_form(
    cx: &Cx,
    columns: &[ColumnChoice],
    existing: Option<&RuleLine>,
    refusal: Option<&Refusal>,
    lang: Lang,
) -> Result {
    let is_edit = existing.is_some();
    let action = if is_edit {
        "/api/update_rule"
    } else {
        "/api/create_rule"
    };
    let trigger_kind = existing
        .map(|rule| rule.trigger_kind.as_str())
        .unwrap_or("status");
    let column_id = existing
        .and_then(|rule| rule.column_id.as_deref())
        .or_else(|| columns.first().map(|column| column.id.as_str()))
        .unwrap_or_default();
    let subject = existing
        .map(|rule| rule.subject.as_str())
        .unwrap_or_default();
    let audience_kind = existing
        .map(|rule| rule.audience_kind.as_str())
        .unwrap_or("assignees");
    let include_task_details = existing.is_some_and(|rule| rule.include_task_details);

    view! {
        cx =>
        <form method="post" action=(action) class="rule-new-body">
            if let Some(rule) = existing {
                <input type="hidden" name="rule_id" value=(rule.id.clone())>
            }
            <label class="rule-field">
                <span class="field-label">(t(lang, Key::WhenLabel))</span>
                <select class="field-input" name="trigger">
                    <option value="status" selected=(trigger_kind == "status")>(t(lang, Key::TriggerStatusBecomes))</option>
                    <option value="unblocked" selected=(trigger_kind == "unblocked")>(t(lang, Key::TriggerUnblocked))</option>
                    <option value="created" selected=(trigger_kind == "created")>(t(lang, Key::TriggerCreated))</option>
                    <option value="assigned" selected=(trigger_kind == "assigned")>(t(lang, Key::TriggerAssigned))</option>
                    <option value="unassigned" selected=(trigger_kind == "unassigned")>(t(lang, Key::TriggerUnassigned))</option>
                    <option value="commented" selected=(trigger_kind == "commented")>(t(lang, Key::TriggerCommented))</option>
                    <option value="deadline_set" selected=(trigger_kind == "deadline_set")>(t(lang, Key::TriggerDeadlineSet))</option>
                    <option value="deadline_cleared" selected=(trigger_kind == "deadline_cleared")>(t(lang, Key::TriggerDeadlineCleared))</option>
                    <option value="retitled" selected=(trigger_kind == "retitled")>(t(lang, Key::TriggerRetitled))</option>
                    <option value="linked" selected=(trigger_kind == "linked")>(t(lang, Key::TriggerLinked))</option>
                    <option value="unlinked" selected=(trigger_kind == "unlinked")>(t(lang, Key::TriggerUnlinked))</option>
                    <option value="deleted" selected=(trigger_kind == "deleted")>(t(lang, Key::TriggerDeleted))</option>
                </select>
            </label>
            if trigger_kind == "status" {
                <label class="rule-field">
                    <span class="field-label">(t(lang, Key::ColumnLabel))</span>
                    <select class="field-input" name="column_id">
                        for column in columns {
                            <option value=(column.id.clone()) selected=(column.id == column_id)>(column.name.clone())</option>
                        }
                    </select>
                </label>
            } else {
                <input type="hidden" name="column_id" value=(column_id)>
            }
            <label class="rule-field">
                <span class="field-label">(t(lang, Key::SendLabel))</span>
                <input
                    class="field-input"
                    type="text"
                    name="subject"
                    placeholder="Task completed"
                    maxlength="120"
                    required=""
                    value=(subject)
                >
            </label>
            <label class="rule-field">
                <span class="field-label">(t(lang, Key::ToLabel))</span>
                <select class="field-input" name="audience">
                    <option value="assignees" selected=(audience_kind == "assignees")>(t(lang, Key::AudienceAssignees))</option>
                    <option value="board" selected=(audience_kind == "board")>(t(lang, Key::AudienceBoard))</option>
                    <option value="creator" selected=(audience_kind == "creator")>(t(lang, Key::AudienceCreator))</option>
                </select>
            </label>
            <div class="rule-field">
                <span class="field-label">(t(lang, Key::BodyLabel))</span>
                <label class="field-box">
                    <input type="checkbox" name="include_task_details" value="true" checked=(include_task_details)>
                    <span class="field-text">(t(lang, Key::TaskDetails))</span>
                </label>
            </div>
            <div class="panel-foot">
                if let Some(refusal) = refusal {
                    <span class="field-error">(refusal.message_in(lang))</span>
                }
                if is_edit {
                    <a class="quiet" href="/rules">(t(lang, Key::Cancel))</a>
                }
                <button class="primary" type="submit">(if is_edit { t(lang, Key::SaveRule) } else { t(lang, Key::AddRule) })</button>
            </div>
        </form>
    }
}

/// The "New rule" control and the form inside it. A `<details>` rather than
/// anything script-driven, so a browser with no script can still open it and
/// post the form.
async fn composer(
    cx: &Cx,
    columns: &[ColumnChoice],
    refusal: Option<&Refusal>,
    lang: Lang,
) -> Result {
    view! {
        cx =>
        <details class="rule-new">
            <summary class="rule-new-open">(t(lang, Key::NewRule))</summary>
            (rule_form(cx, columns, None, refusal, lang).await?)
        </details>
    }
}

/// One rule's row: the form in place of it when `editing`, its display
/// otherwise.
async fn rule_row(
    cx: &Cx,
    rule: &RuleLine,
    columns: &[ColumnChoice],
    editing: bool,
    refusal: Option<&Refusal>,
    lang: Lang,
) -> Result {
    if editing {
        return view! {
            cx =>
            <div class="rule-row">
                (rule_form(cx, columns, Some(rule), refusal, lang).await?)
            </div>
        };
    }

    let row_class = if rule.enabled {
        "rule-row"
    } else {
        "rule-row rule-row-off"
    };
    let switch_class = if rule.enabled {
        "rule-switch rule-switch-on"
    } else {
        "rule-switch"
    };
    let switch_label = if rule.enabled {
        t(lang, Key::SwitchRuleOff)
    } else {
        t(lang, Key::SwitchRuleOn)
    };
    let stamp = rule
        .last_sent
        .as_ref()
        .map(|moment| crate::i18n::last_sent_label(lang, moment))
        .or_else(|| {
            rule.last_fired
                .as_ref()
                .map(|moment| crate::i18n::last_fired_label(lang, moment))
        })
        .unwrap_or_else(|| t(lang, Key::NeverFired).to_string());

    view! {
        cx =>
        <div class=(row_class)>
            <form method="post" action="/api/set_rule_enabled" class="rule-switch-form">
                <input type="hidden" name="rule_id" value=(rule.id.clone())>
                <input type="hidden" name="enabled" value=((!rule.enabled).to_string())>
                <button class=(switch_class) type="submit" title=(switch_label)>
                    <span class="rule-switch-knob"></span>
                    <span class="visually-hidden">(switch_label)</span>
                </button>
            </form>

            <div class="rule-sentence">
                <span>(rule.when.clone())</span>
                <span class="rule-term">(rule.what.clone())</span>
                <span>(t(lang, Key::SendConnector))</span>
                <span class="rule-term">(rule.subject.clone())</span>
                <span>(t(lang, Key::ToConnector))</span>
                <span class="rule-term">(rule.audience.clone())</span>
            </div>

            <span class="rule-stamp">(stamp)</span>

            <a class="quiet" href=(format!("/rules?edit={}", rule.id)) title=(t(lang, Key::EditThisRule))>(t(lang, Key::EditLabel))</a>

            <form method="post" action="/api/delete_rule">
                <input type="hidden" name="rule_id" value=(rule.id.clone())>
                <button class="quiet quiet-danger" type="submit" title=(t(lang, Key::DeleteThisRule))>(t(lang, Key::Delete))</button>
            </form>
        </div>
    }
}

#[page("/rules")]
async fn rules_page(cx: &Cx) -> Result {
    let user = match require_admin(cx).await {
        Ok(user) => user,
        Err(refusal) => {
            return view! {
                <main class="scaffold-note">
                    <p>(refusal.message())</p>
                    <p><a href="/">(t(Lang::En, Key::BackToBoard))</a></p>
                </main>
            };
        }
    };
    let lang = Lang::from_code(&user.language);
    let store = accounts(cx).store().clone();
    let snapshot = match snapshot_of(&store, &user).await {
        Ok(snapshot) => snapshot,
        Err(refusal) => {
            return view! {
                <main class="scaffold-note">
                    <p>(refusal.message_in(lang))</p>
                    <p><a href="/">(t(lang, Key::BackToBoard))</a></p>
                </main>
            };
        }
    };
    let edit_id = query_params::<RulesQuery>(cx)?.edit.clone();
    let create_refusal = refusal_of(cx, "create_rule");
    let update_refusal = refusal_of(cx, "update_rule");

    view! {
        <header class="topbar">
            <a class="wordmark" href="/">
                <span class="wordmark-text">"izlek"</span>
                <span class="wordmark-dot"></span>
            </a>
            (crate::layout::topbar_nav(cx, crate::layout::NavPage::Rules, user.role, lang).await?)
            <div class="spacer"></div>
            (crate::layout::user_menu(cx, &crate::detail::Me::from(&user), lang).await?)
        </header>

        <div class="settings-shell">
            <main class="settings-stage">
                <div class="settings-head">
                    <h1 class="settings-title">(t(lang, Key::MailRules))</h1>
                    <span class="chip chip-admin">(t(lang, Key::AdminOnly))</span>
                </div>

                if !snapshot.sender_connected {
                    <p class="rules-quiet">
                        (t(lang, Key::NoSenderConnectedPrefix))
                        (" ")
                        <a href="/settings">(t(lang, Key::NavSettings))</a>
                    </p>
                }

                (composer(cx, &snapshot.columns, create_refusal.as_ref(), lang).await?)

                <div class="rule-list">
                    for rule in &snapshot.rules {
                        (rule_row(cx, rule, &snapshot.columns, edit_id.as_deref() == Some(rule.id.as_str()), update_refusal.as_ref(), lang).await?)
                    }
                    if snapshot.rules.is_empty() {
                        <p class="rules-quiet">(t(lang, Key::NoRulesYet))</p>
                    }
                </div>
            </main>
            (crate::dropdown::dropdown_script(cx).await?)
            (crate::layout::escape_script(cx).await?)
        </div>
    }
}
