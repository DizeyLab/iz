//! Mail rules, from the MailRules artboard.
//!
//! The screen is the admin's, and so is every call behind it: a rule sends mail
//! on everyone's behalf, from the one workspace sender, so writing one is not
//! something a Member may do even if they reach the endpoint directly. The
//! chip in the artboard says "Admin only"; the guard is in the handlers.

use leptos::prelude::*;
use serde::{Deserialize, Serialize};

use crate::auth::{Me, Refusal};

/// One rule as the screen reads it: a sentence, a switch and a stamp.
///
/// The trigger arrives already split into the words the artboard prints —
/// "When status becomes" / "Done" — because the two halves are chosen from
/// different vocabularies and only the server knows the column names.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleLine {
    pub id: String,
    /// "When status becomes", or "When a task stops being".
    pub when: String,
    /// "Done", or "blocked", or empty when the trigger has no second half.
    pub what: String,
    pub subject: String,
    /// "assignees", "everyone on board", or "its creator".
    pub audience: String,
    /// The trigger's own word — "status", "unblocked", "created" — as the
    /// edit form's select expects it, not as the sentence prints it.
    pub trigger_kind: String,
    /// The named column, for a status trigger's edit form. `None` for every
    /// other trigger.
    pub column_id: Option<String>,
    /// The audience's own word — "assignees", "board", "creator" — as the
    /// edit form's select expects it.
    pub audience_kind: String,
    pub enabled: bool,
    /// `Aug 24 14:02`, or nothing when no mail from this rule has ever been
    /// accepted by the mail server.
    pub last_sent: Option<String>,
    /// `Aug 24 14:02` of the rule's last decision, sent or not, or nothing
    /// when the rule has never been evaluated.
    pub last_fired: Option<String>,
}

/// A column, for the composer's status list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnChoice {
    pub id: String,
    pub name: String,
}

/// The screen in one answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RulesSnapshot {
    pub me: Me,
    pub rules: Vec<RuleLine>,
    pub columns: Vec<ColumnChoice>,
    /// Whether a sender is connected. Rules that fire with no sender queue
    /// rather than fail, and the screen says so instead of leaving somebody to
    /// wonder why nothing arrived.
    pub sender_connected: bool,
}

/// The word for an audience, as the artboard writes it.
#[cfg(feature = "ssr")]
fn audience_word(audience: izlek_core::store::Audience) -> &'static str {
    use izlek_core::store::Audience;
    match audience {
        Audience::Assignees => "assignees",
        Audience::Board => "everyone on board",
        Audience::Creator => "its creator",
    }
}

/// The audience's own word, as the edit form's select names it.
#[cfg(feature = "ssr")]
fn audience_kind(audience: izlek_core::store::Audience) -> &'static str {
    use izlek_core::store::Audience;
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
#[cfg(feature = "ssr")]
fn trigger_of(kind: &str, column_id: Option<String>) -> Option<izlek_core::store::Trigger> {
    use izlek_core::store::Trigger;
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

/// Every rule on the board, switched-off ones included, with the columns a new
/// rule may name.
#[server]
pub async fn current_rules() -> Result<Result<RulesSnapshot, Refusal>, ServerFnError> {
    use crate::server::{accounts, require_admin};
    use izlek_core::store::Trigger;

    let user = match require_admin().await {
        Ok(user) => user,
        Err(refusal) => return Ok(Err(refusal)),
    };
    let store = accounts().store().clone();
    let fail = |e: izlek_core::store::StoreError| ServerFnError::new(e.to_string());
    let Some(board) = store.board(&user.workspace_id).await.map_err(fail)? else {
        return Ok(Err(Refusal::Unavailable));
    };
    let columns = store.columns(&board.id).await.map_err(fail)?;
    let rules = store.mail_rules(&board.id).await.map_err(fail)?;
    let last_sent = store.mail_rule_last_sent(&board.id).await.map_err(fail)?;
    let last_decision = store.mail_rule_last_decision().await.map_err(fail)?;
    let sender_connected = store
        .workspace()
        .await
        .map_err(fail)?
        .as_ref()
        .map(crate::settings::Sender::of)
        .is_some_and(|sender| sender.is_connected());

    let lines = rules
        .into_iter()
        .map(|rule| {
            let (when, what, trigger_kind, column_id) = match &rule.trigger {
                Trigger::StatusBecomes(column_id) => (
                    "When status becomes".to_string(),
                    columns
                        .iter()
                        .find(|column| &column.id == column_id)
                        .map(|column| column.name.clone())
                        // A rule can outlive the column it names. Saying so is
                        // better than printing an id nobody can read.
                        .unwrap_or_else(|| "a column that is gone".to_string()),
                    "status".to_string(),
                    Some(column_id.clone()),
                ),
                Trigger::Unblocked => (
                    "When a task stops being".to_string(),
                    "blocked".to_string(),
                    "unblocked".to_string(),
                    None,
                ),
                Trigger::Created => {
                    ("When a task is created".to_string(), String::new(), "created".to_string(), None)
                }
                Trigger::Assigned => {
                    ("When someone is assigned".to_string(), String::new(), "assigned".to_string(), None)
                }
                Trigger::Unassigned => (
                    "When someone is unassigned".to_string(),
                    String::new(),
                    "unassigned".to_string(),
                    None,
                ),
                Trigger::Commented => (
                    "When a comment is written".to_string(),
                    String::new(),
                    "commented".to_string(),
                    None,
                ),
                Trigger::DeadlineSet => (
                    "When a deadline is set".to_string(),
                    String::new(),
                    "deadline_set".to_string(),
                    None,
                ),
                Trigger::DeadlineCleared => (
                    "When a deadline is removed".to_string(),
                    String::new(),
                    "deadline_cleared".to_string(),
                    None,
                ),
                Trigger::Retitled => {
                    ("When a task is renamed".to_string(), String::new(), "retitled".to_string(), None)
                }
                Trigger::Linked => {
                    ("When a task is linked".to_string(), String::new(), "linked".to_string(), None)
                }
                Trigger::Unlinked => (
                    "When a task is unlinked".to_string(),
                    String::new(),
                    "unlinked".to_string(),
                    None,
                ),
                Trigger::Deleted => {
                    ("When a task is deleted".to_string(), String::new(), "deleted".to_string(), None)
                }
            };
            RuleLine {
                when,
                what,
                subject: rule.subject.clone(),
                audience: audience_word(rule.audience).to_string(),
                trigger_kind,
                column_id,
                audience_kind: audience_kind(rule.audience).to_string(),
                enabled: rule.enabled,
                last_sent: last_sent
                    .iter()
                    .find(|(id, _)| id == &rule.id)
                    .map(|(_, at)| izlek_core::detail::moment_label(*at)),
                last_fired: last_decision
                    .iter()
                    .find(|(id, _)| id == &rule.id)
                    .map(|(_, at)| izlek_core::detail::moment_label(*at)),
                id: rule.id,
            }
        })
        .collect();

    Ok(Ok(RulesSnapshot {
        me: Me {
            id: user.id,
            display_name: user.display_name,
            email: user.email,
            role: user.role,
        },
        rules: lines,
        columns: columns
            .into_iter()
            .map(|column| ColumnChoice { id: column.id, name: column.name })
            .collect(),
        sender_connected,
    }))
}

/// Writes one rule.
///
/// `trigger` is "status" or "unblocked"; `column_id` is only read for the
/// first, and is checked against this workspace's own board rather than
/// trusted — a column id from somewhere else would otherwise be a way to hang
/// a rule off another workspace's board.
#[server]
pub async fn create_rule(
    trigger: String,
    column_id: String,
    subject: String,
    audience: String,
) -> Result<Option<Refusal>, ServerFnError> {
    use crate::server::{accounts, require_admin};
    use izlek_core::store::Audience;

    let user = match require_admin().await {
        Ok(user) => user,
        Err(refusal) => return Ok(Some(refusal)),
    };
    let subject = subject.trim().to_string();
    if subject.is_empty() {
        return Ok(Some(Refusal::EmptySubject));
    }
    let audience = match audience.as_str() {
        "assignees" => Audience::Assignees,
        "board" => Audience::Board,
        "creator" => Audience::Creator,
        _ => return Ok(Some(Refusal::Forbidden)),
    };

    let store = accounts().store().clone();
    let fail = |e: izlek_core::store::StoreError| ServerFnError::new(e.to_string());
    let Some(board) = store.board(&user.workspace_id).await.map_err(fail)? else {
        return Ok(Some(Refusal::Unavailable));
    };
    let column_id = if column_id.trim().is_empty() { None } else { Some(column_id) };
    if trigger == "status" {
        let Some(column_id) = &column_id else { return Ok(Some(Refusal::Forbidden)) };
        let columns = store.columns(&board.id).await.map_err(fail)?;
        if !columns.iter().any(|column| &column.id == column_id) {
            return Ok(Some(Refusal::Forbidden));
        }
    }
    let Some(trigger) = trigger_of(&trigger, column_id) else {
        return Ok(Some(Refusal::Forbidden));
    };

    store
        .create_mail_rule(
            &board.id,
            &trigger,
            &subject,
            audience,
            time::OffsetDateTime::now_utc(),
        )
        .await
        .map_err(fail)?;
    Ok(None)
}

/// Rewrites a rule's sentence in place. Guarded exactly like `create_rule` —
/// same subject rule, same column-belongs-to-this-board check — plus the
/// rule itself has to be this workspace's, or an id from elsewhere could be
/// rewritten by an admin who never owned it.
#[server]
pub async fn update_rule(
    rule_id: String,
    trigger: String,
    column_id: String,
    subject: String,
    audience: String,
) -> Result<Option<Refusal>, ServerFnError> {
    use crate::server::{accounts, require_admin};
    use izlek_core::store::Audience;

    let user = match require_admin().await {
        Ok(user) => user,
        Err(refusal) => return Ok(Some(refusal)),
    };
    let subject = subject.trim().to_string();
    if subject.is_empty() {
        return Ok(Some(Refusal::EmptySubject));
    }
    let audience = match audience.as_str() {
        "assignees" => Audience::Assignees,
        "board" => Audience::Board,
        "creator" => Audience::Creator,
        _ => return Ok(Some(Refusal::Forbidden)),
    };

    let store = accounts().store().clone();
    let fail = |e: izlek_core::store::StoreError| ServerFnError::new(e.to_string());
    let Some(board) = store.board(&user.workspace_id).await.map_err(fail)? else {
        return Ok(Some(Refusal::Unavailable));
    };
    let rule = store.mail_rule(&rule_id).await.map_err(fail)?;
    match rule {
        Some(rule) if rule.board_id == board.id => {}
        _ => return Ok(Some(Refusal::NotFound)),
    }

    let column_id = if column_id.trim().is_empty() { None } else { Some(column_id) };
    if trigger == "status" {
        let Some(column_id) = &column_id else { return Ok(Some(Refusal::Forbidden)) };
        let columns = store.columns(&board.id).await.map_err(fail)?;
        if !columns.iter().any(|column| &column.id == column_id) {
            return Ok(Some(Refusal::Forbidden));
        }
    }
    let Some(trigger) = trigger_of(&trigger, column_id) else {
        return Ok(Some(Refusal::Forbidden));
    };

    store
        .update_mail_rule(&rule_id, &trigger, &subject, audience)
        .await
        .map_err(fail)?;
    Ok(None)
}

/// Turns one rule on or off. Switching off stops it firing from here on; mail
/// it already owes has been written to the ledger and is still owed.
#[server]
pub async fn set_rule_enabled(
    rule_id: String,
    enabled: String,
) -> Result<Option<Refusal>, ServerFnError> {
    let store = match rule_of_this_workspace(&rule_id).await {
        Ok(store) => store,
        Err(refusal) => return Ok(Some(refusal)),
    };
    store
        .set_mail_rule_enabled(&rule_id, enabled == "true")
        .await
        .map_err(|e| ServerFnError::new(e.to_string()))?;
    Ok(None)
}

/// Removes a rule. What it has already sent stays in the ledger: the rule is
/// gone, the record of what went out is not.
#[server]
pub async fn delete_rule(rule_id: String) -> Result<Option<Refusal>, ServerFnError> {
    let store = match rule_of_this_workspace(&rule_id).await {
        Ok(store) => store,
        Err(refusal) => return Ok(Some(refusal)),
    };
    store
        .delete_mail_rule(&rule_id)
        .await
        .map_err(|e| ServerFnError::new(e.to_string()))?;
    Ok(None)
}

/// The admin's store, once the rule id has been shown to name a rule on this
/// workspace's own board.
///
/// A rule id is opaque and arrives from the browser, so "not yours" and "not a
/// rule" are the same answer: neither tells the caller whether the id exists
/// somewhere else.
#[cfg(feature = "ssr")]
async fn rule_of_this_workspace(
    rule_id: &str,
) -> Result<std::sync::Arc<dyn izlek_core::store::Store>, Refusal> {
    use crate::server::{accounts, require_admin};

    let user = require_admin().await?;
    let store = accounts().store().clone();
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
    Ok(store)
}

#[component]
pub fn RulesPage() -> impl IntoView {
    let rules = Resource::new(|| (), |_| async move { current_rules().await });

    view! {
        <Transition fallback=|| view! { <main class="settings-stage"></main> }>
            {move || Suspend::new(async move {
                match rules.await {
                    Ok(Ok(snapshot)) => {
                        view! {
                            <RulesScreen
                                snapshot=snapshot
                                on_change=Callback::new(move |()| rules.refetch())
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
fn RulesScreen(snapshot: RulesSnapshot, on_change: Callback<()>) -> impl IntoView {
    let me = snapshot.me.clone();
    let sender_connected = snapshot.sender_connected;
    let empty = snapshot.rules.is_empty();
    let columns = snapshot.columns.clone();
    let rows = snapshot
        .rules
        .into_iter()
        .map(|rule| {
            view! { <RuleRow rule=rule columns=columns.clone() on_change=on_change/> }
        })
        .collect_view();

    view! {
        <header class="topbar">
            <div class="wordmark">
                <span class="wordmark-text">"izlek"</span>
                <span class="wordmark-dot"></span>
            </div>
            <div class="topbar-divider"></div>
            <span class="board-name">"Mail rules"</span>
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
                <a class="sidenav-item sidenav-item-on" href="/rules">
                    "Mail rules"
                </a>
                <a class="sidenav-item" href="/logs">
                    "Logs"
                </a>
                <a class="sidenav-item" href="/settings">
                    "Settings"
                </a>
            </nav>

            <main class="settings-stage">
                <div class="settings-head">
                    <h1 class="settings-title">"Mail rules"</h1>
                    <span class="chip chip-admin">"Admin only"</span>
                </div>

                {(!sender_connected)
                    .then(|| {
                        view! {
                            <p class="rules-quiet">
                                "No sender connected — mail waits in the queue until you set one in "
                                <a href="/settings">"Settings"</a>
                                "."
                            </p>
                        }
                    })}

                <Composer columns=snapshot.columns on_change=on_change/>

                <div class="rule-list">
                    {rows}
                    {empty.then(|| view! { <p class="rules-quiet">"No rules yet."</p> })}
                </div>
            </main>
        </div>
    }
}

/// The "New rule" control and the sentence it builds.
///
/// It is a `<details>` rather than a signal-driven panel so that a browser with
/// no script can still open it and post the form.
#[component]
fn Composer(columns: Vec<ColumnChoice>, on_change: Callback<()>) -> impl IntoView {
    // The composer owns its own open state rather than letting `<details>`
    // keep it: a rule that has just been written should leave a closed, empty
    // composer behind, not the sentence you already sent sitting in the field.
    let open = RwSignal::new(false);

    view! {
        <details class="rule-new" open=move || open.get()>
            <summary
                class="rule-new-open"
                on:click=move |ev| {
                    ev.prevent_default();
                    open.update(|o| *o = !*o);
                }
            >
                "New rule"
            </summary>
            <RuleForm
                columns=columns
                existing=None
                on_change=on_change
                on_saved=Some(Callback::new(move |()| open.set(false)))
                on_cancel=None
            />
        </details>
    }
}

/// The trigger/column/subject/audience form: one component for both writing a
/// new rule and rewriting an existing one, so the two never drift apart.
///
/// `existing` is `None` for a fresh rule and `Some` to edit one in place —
/// the only difference the two paths need beyond which action they post to
/// and what starts in the fields.
#[component]
fn RuleForm(
    columns: Vec<ColumnChoice>,
    existing: Option<RuleLine>,
    on_change: Callback<()>,
    /// Extra to do once the write lands, beyond refetching — the composer
    /// closes its `<details>`; an edit row has nothing more to do, since the
    /// refetch rebuilds it back to its closed state on its own.
    on_saved: Option<Callback<()>>,
    /// The Cancel button, present only for an edit in progress.
    on_cancel: Option<Callback<()>>,
) -> impl IntoView {
    let is_edit = existing.is_some();
    let rule_id = existing.as_ref().map(|rule| rule.id.clone()).unwrap_or_default();
    let first_column = columns.first().map(|column| column.id.clone()).unwrap_or_default();

    let trigger_kind = RwSignal::new(
        existing.as_ref().map(|rule| rule.trigger_kind.clone()).unwrap_or_else(|| "status".to_string()),
    );
    let column_id = RwSignal::new(
        existing.as_ref().and_then(|rule| rule.column_id.clone()).unwrap_or(first_column),
    );
    let subject = RwSignal::new(existing.as_ref().map(|rule| rule.subject.clone()).unwrap_or_default());
    let audience_kind = RwSignal::new(
        existing.as_ref().map(|rule| rule.audience_kind.clone()).unwrap_or_else(|| "assignees".to_string()),
    );

    let create = ServerAction::<CreateRule>::new();
    let update = ServerAction::<UpdateRule>::new();
    let create_value = create.value();
    let update_value = update.value();
    Effect::new(move |_| {
        if matches!(create_value.get(), Some(Ok(None))) {
            subject.set(String::new());
            on_change.run(());
            if let Some(on_saved) = on_saved {
                on_saved.run(());
            }
        }
    });
    Effect::new(move |_| {
        if matches!(update_value.get(), Some(Ok(None))) {
            on_change.run(());
        }
    });
    let refusal = move || {
        let value = if is_edit { update_value.get() } else { create_value.get() };
        match value {
            Some(Ok(Some(refusal))) => Some(refusal.message()),
            Some(Err(_)) => Some(Refusal::Unavailable.message()),
            _ => None,
        }
    };

    let options = columns
        .into_iter()
        .map(|column| view! { <option value=column.id>{column.name}</option> })
        .collect_view();

    let fields = view! {
        <label class="rule-field">
            <span class="field-label">"WHEN"</span>
            <select
                class="field-input"
                name="trigger"
                prop:value=move || trigger_kind.get()
                on:change=move |ev| trigger_kind.set(event_target_value(&ev))
            >
                <option value="status">"status becomes"</option>
                <option value="unblocked">"a task stops being blocked"</option>
                <option value="created">"a task is created"</option>
                <option value="assigned">"someone is assigned"</option>
                <option value="unassigned">"someone is unassigned"</option>
                <option value="commented">"a comment is written"</option>
                <option value="deadline_set">"a deadline is set"</option>
                <option value="deadline_cleared">"a deadline is removed"</option>
                <option value="retitled">"a task is renamed"</option>
                <option value="linked">"a task is linked"</option>
                <option value="unlinked">"a task is unlinked"</option>
                <option value="deleted">"a task is deleted"</option>
            </select>
        </label>
        <input type="hidden" name="column_id" value=move || column_id.get()/>
        {move || {
            (trigger_kind.get() == "status")
                .then(|| {
                    view! {
                        <label class="rule-field">
                            <span class="field-label">"COLUMN"</span>
                            <select
                                class="field-input"
                                prop:value=move || column_id.get()
                                on:change=move |ev| column_id.set(event_target_value(&ev))
                            >
                                {options.clone()}
                            </select>
                        </label>
                    }
                })
        }}
        <label class="rule-field">
            <span class="field-label">"SEND"</span>
            <input
                class="field-input"
                type="text"
                name="subject"
                placeholder="Task completed"
                maxlength="120"
                required
                prop:value=move || subject.get()
                on:input=move |ev| subject.set(event_target_value(&ev))
            />
        </label>
        <label class="rule-field">
            <span class="field-label">"TO"</span>
            <select
                class="field-input"
                name="audience"
                prop:value=move || audience_kind.get()
                on:change=move |ev| audience_kind.set(event_target_value(&ev))
            >
                <option value="assignees">"assignees"</option>
                <option value="board">"everyone on board"</option>
                <option value="creator">"its creator"</option>
            </select>
        </label>
        <div class="panel-foot">
            {move || {
                refusal()
                    .map(|message| view! { <span class="field-error">{message}</span> })
            }}
            {on_cancel
                .map(|on_cancel| {
                    view! {
                        <button
                            class="rule-delete"
                            type="button"
                            on:click=move |_| on_cancel.run(())
                        >
                            "Cancel"
                        </button>
                    }
                })}
            <button class="primary" type="submit">
                {if is_edit { "Save rule" } else { "Add rule" }}
            </button>
        </div>
    };

    if is_edit {
        view! {
            <ActionForm action=update attr:class="rule-new-body">
                <input type="hidden" name="rule_id" value=rule_id.clone()/>
                {fields}
            </ActionForm>
        }
            .into_any()
    } else {
        view! { <ActionForm action=create attr:class="rule-new-body">{fields}</ActionForm> }.into_any()
    }
}

#[component]
fn RuleRow(rule: RuleLine, columns: Vec<ColumnChoice>, on_change: Callback<()>) -> impl IntoView {
    let editing = RwSignal::new(false);
    let toggle = ServerAction::<SetRuleEnabled>::new();
    let remove = ServerAction::<DeleteRule>::new();
    let toggled = toggle.value();
    Effect::new(move |_| {
        if matches!(toggled.get(), Some(Ok(None))) {
            on_change.run(());
        }
    });
    let removed = remove.value();
    Effect::new(move |_| {
        if matches!(removed.get(), Some(Ok(None))) {
            on_change.run(());
        }
    });

    let for_form = rule.clone();
    let RuleLine { id, when, what, subject, audience, enabled, last_sent, last_fired, .. } = rule;
    let stamp = last_sent
        .map(|moment| format!("last sent {moment}"))
        .or_else(|| last_fired.map(|moment| format!("last fired {moment}")))
        .unwrap_or_else(|| "never fired".to_string());
    let row_class = if enabled { "rule-row" } else { "rule-row rule-row-off" };
    // Each form carries the id in its own hidden field, and each of those
    // fields owns its copy: the two live in separate closures.
    let toggle_id = id.clone();
    let delete_id = id;

    view! {
        <div class=row_class>
            {move || {
                if editing.get() {
                    view! {
                        <RuleForm
                            columns=columns.clone()
                            existing=Some(for_form.clone())
                            on_change=on_change
                            on_saved=None
                            on_cancel=Some(Callback::new(move |()| editing.set(false)))
                        />
                    }
                        .into_any()
                } else {
                    view! {
                        <RuleRowDisplay
                            toggle_id=toggle_id.clone()
                            delete_id=delete_id.clone()
                            when=when.clone()
                            what=what.clone()
                            subject=subject.clone()
                            audience=audience.clone()
                            stamp=stamp.clone()
                            enabled=enabled
                            toggle=toggle
                            remove=remove
                            on_edit=Callback::new(move |()| editing.set(true))
                        />
                    }
                        .into_any()
                }
            }}
        </div>
    }
}

/// The row as it reads when nobody is editing it — its own component so the
/// attributes inside are built once per showing rather than re-entering the
/// reactive closure that switches it in and out for an edit.
#[component]
fn RuleRowDisplay(
    toggle_id: String,
    delete_id: String,
    when: String,
    what: String,
    subject: String,
    audience: String,
    stamp: String,
    enabled: bool,
    toggle: ServerAction<SetRuleEnabled>,
    remove: ServerAction<DeleteRule>,
    on_edit: Callback<()>,
) -> impl IntoView {
    let switch_class = if enabled { "rule-switch rule-switch-on" } else { "rule-switch" };
    let switch_label = if enabled { "Switch this rule off" } else { "Switch this rule on" };

    view! {
        <ActionForm action=toggle attr:class="rule-switch-form">
            <input type="hidden" name="rule_id" value=toggle_id/>
            <input type="hidden" name="enabled" value=(!enabled).to_string()/>
            <button class=switch_class type="submit" title=switch_label>
                <span class="rule-switch-knob"></span>
                <span class="visually-hidden">{switch_label}</span>
            </button>
        </ActionForm>

        <div class="rule-sentence">
            <span>{when}</span>
            <span class="rule-term">{what}</span>
            <span>"send"</span>
            <span class="rule-term">{subject}</span>
            <span>"to"</span>
            <span class="rule-term">{audience}</span>
        </div>

        <span class="rule-stamp">{stamp}</span>

        <button class="rule-delete" type="button" title="Edit this rule" on:click=move |_| on_edit.run(())>
            "Edit"
        </button>

        <ActionForm action=remove>
            <input type="hidden" name="rule_id" value=delete_id/>
            <button class="rule-delete" type="submit" title="Delete this rule">
                "Delete"
            </button>
        </ActionForm>
    }
}
