//! The account screens, from the FirstLogin artboard: setting up the first
//! account, an invited member picking a password, and signing in.

use leptos::prelude::*;
use leptos_router::hooks::use_params_map;

use crate::auth::{
    ClaimWorkspace, Gate, Invited, RedeemLink, Refusal, SignIn, current_gate, invitation,
};
use crate::board::Board;

/// The front door. Which screen this is depends on the workspace, and the
/// server decides — an empty workspace offers setup, a claimed one offers a
/// sign-in, a signed-in browser gets the board.
#[component]
pub fn Landing() -> impl IntoView {
    let gate = Resource::new(|| (), |_| async move { current_gate().await });

    view! {
        <Transition fallback=|| view! { <main class="auth-stage"></main> }>
            {move || Suspend::new(async move {
                match gate.await {
                    Ok(Gate::NeedsSetup) => {
                        view! {
                            <Topbar/>
                            <SetupCard on_done=move || gate.refetch()/>
                        }
                            .into_any()
                    }
                    Ok(Gate::NeedsSignIn) => {
                        view! {
                            <Topbar/>
                            <SignInCard on_done=move || gate.refetch()/>
                        }
                            .into_any()
                    }
                    Ok(Gate::SignedIn(_)) => view! { <Board/> }.into_any(),
                    Err(_) => {
                        view! {
                            <Topbar/>
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
fn Topbar() -> impl IntoView {
    view! {
        <header class="topbar">
            <div class="wordmark">
                <span class="wordmark-text">"izlek"</span>
                <span class="wordmark-dot"></span>
            </div>
        </header>
    }
}

/// "Set up Izlek" — the first account in an empty workspace.
#[component]
fn SetupCard(on_done: impl Fn() + Copy + Send + Sync + 'static) -> impl IntoView {
    let action = ServerAction::<ClaimWorkspace>::new();
    let value = action.value();
    Effect::new(move |_| {
        if matches!(value.get(), Some(Ok(None))) {
            on_done();
        }
    });

    view! {
        <main class="auth-stage">
            <div class="auth-column">
                <span class="auth-kicker">"FIRST ACCOUNT — EMPTY WORKSPACE"</span>
                <div class="auth-card">
                    <div class="auth-head">
                        <div class="auth-title">"Set up Izlek"</div>
                        <div class="auth-sub">
                            "Nobody has signed in yet, so this account administers the workspace: the mail sender, the limits and the member list are yours."
                        </div>
                    </div>
                    <ActionForm action=action>
                        <label class="auth-field">
                            <span class="auth-label">"YOUR NAME"</span>
                            <input
                                class="auth-input"
                                type="text"
                                name="display_name"
                                autocomplete="name"
                                required
                            />
                        </label>
                        <label class="auth-field">
                            <span class="auth-label">"EMAIL"</span>
                            <input
                                class="auth-input auth-input-mono"
                                type="email"
                                name="email"
                                autocomplete="email"
                                required
                            />
                        </label>
                        <label class="auth-field">
                            <span class="auth-label">"PASSWORD"</span>
                            <input
                                class="auth-input auth-input-mono"
                                type="password"
                                name="password"
                                autocomplete="new-password"
                                required
                            />
                        </label>
                        <PasswordRules/>
                        <button class="auth-submit" type="submit" disabled=move || action.pending().get()>
                            <span class="auth-submit-text">"Create workspace"</span>
                            <span class="auth-key">"↵"</span>
                        </button>
                    </ActionForm>
                    {problem(action)}
                    <div class="auth-foot">
                        "Mail rules stay quiet until you connect a sender in Settings. Nothing leaves the machine before that."
                    </div>
                </div>
            </div>
        </main>
    }
}

/// "Pick a password" — the invited member's first sign-in, reached from the
/// emailed link. The address was set by the admin and cannot be edited here.
#[component]
pub fn Join() -> impl IntoView {
    let params = use_params_map();
    let token = move || params.read().get("token").unwrap_or_default();
    let who = Resource::new(token, |token| async move { invitation(token).await });

    view! {
        <Transition fallback=|| view! { <main class="auth-stage"></main> }>
            {move || Suspend::new(async move {
                match who.await {
                    Ok(Some(person)) => view! { <JoinCard token=token() person=person/> }.into_any(),
                    _ => {
                        view! {
                            <main class="auth-stage">
                                <div class="auth-column">
                                    <div class="auth-card">
                                        <div class="auth-title">"This link no longer works"</div>
                                        <div class="auth-sub">"Sign-in links last seven days."</div>
                                    </div>
                                </div>
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
fn JoinCard(token: String, person: Invited) -> impl IntoView {
    let action = ServerAction::<RedeemLink>::new();
    let value = action.value();
    let navigate = leptos_router::hooks::use_navigate();
    Effect::new(move |_| {
        if matches!(value.get(), Some(Ok(None))) {
            navigate("/", Default::default());
        }
    });

    let repeat = RwSignal::new(String::new());
    let chosen = RwSignal::new(String::new());
    let matches_yet = move || {
        let repeat = repeat.get();
        !repeat.is_empty() && repeat == chosen.get()
    };
    // Who made the account, when that is still knowable. The fallback names no
    // one rather than naming the wrong person: the invitee's own name used to
    // sit here, greeting them with themselves.
    let made_by = match person.invited_by.clone() {
        Some(admin) => format!("{admin} made you an account."),
        None => "An admin made you an account.".to_string(),
    };

    view! {
        <main class="auth-stage">
            <div class="auth-column">
                <span class="auth-kicker">"INVITED MEMBER — FIRST SIGN-IN"</span>
                <div class="auth-card">
                    <div class="auth-head">
                        <div class="auth-title">"Pick a password"</div>
                        <div class="auth-sub">
                            {made_by}
                        </div>
                    </div>
                    <div class="auth-field">
                        <span class="auth-label">"SIGNING IN AS"</span>
                        <div class="auth-locked">
                            <span class="auth-locked-value">{person.email}</span>
                            <span class="auth-locked-note">"set by the admin"</span>
                        </div>
                    </div>
                    <ActionForm action=action>
                        <input type="hidden" name="token" value=token/>
                        <label class="auth-field">
                            <span class="auth-label">"NEW PASSWORD"</span>
                            <input
                                class="auth-input auth-input-mono"
                                type="password"
                                name="password"
                                autocomplete="new-password"
                                required
                                on:input=move |ev| chosen.set(event_target_value(&ev))
                            />
                        </label>
                        <label class="auth-field">
                            <span class="auth-label">"REPEAT IT"</span>
                            <input
                                class="auth-input auth-input-mono"
                                type="password"
                                autocomplete="new-password"
                                on:input=move |ev| repeat.set(event_target_value(&ev))
                            />
                            <Show when=move || !repeat.get().is_empty() && !matches_yet()>
                                <span class="auth-warn">"not the same yet"</span>
                            </Show>
                        </label>
                        <PasswordRules/>
                        <button
                            class="auth-submit"
                            type="submit"
                            disabled=move || !matches_yet() || action.pending().get()
                        >
                            <span class="auth-submit-text">"Set password and sign in"</span>
                        </button>
                        <Show when=move || !matches_yet()>
                            <div class="auth-warn">"Waiting for the repeated password to match."</div>
                        </Show>
                    </ActionForm>
                    {problem(action)}
                    <div class="auth-foot">
                        "Name and photo can wait — you land on the board straight after this. The admin cannot see or set your password."
                    </div>
                </div>
            </div>
        </main>
    }
}

/// The sign-in form for an account that already has a password. It answers the
/// same whether the address is unknown, has no password yet, or the password is
/// wrong — the difference is not the browser's business.
#[component]
fn SignInCard(on_done: impl Fn() + Copy + Send + Sync + 'static) -> impl IntoView {
    let action = ServerAction::<SignIn>::new();
    let value = action.value();
    Effect::new(move |_| {
        if matches!(value.get(), Some(Ok(None))) {
            on_done();
        }
    });

    view! {
        <main class="auth-stage">
            <div class="auth-column">
                <span class="auth-kicker">"SIGN IN"</span>
                <div class="auth-card">
                    <div class="auth-head">
                        <div class="auth-title">"Sign in to Izlek"</div>
                        <div class="auth-sub">
                            "Accounts are made by the admin. If you were invited, use the link you were sent — it is where you choose your password."
                        </div>
                    </div>
                    <ActionForm action=action>
                        <label class="auth-field">
                            <span class="auth-label">"EMAIL"</span>
                            <input
                                class="auth-input auth-input-mono"
                                type="email"
                                name="email"
                                autocomplete="username"
                                required
                            />
                        </label>
                        <label class="auth-field">
                            <span class="auth-label">"PASSWORD"</span>
                            <input
                                class="auth-input auth-input-mono"
                                type="password"
                                name="password"
                                autocomplete="current-password"
                                required
                            />
                        </label>
                        <button class="auth-submit" type="submit" disabled=move || action.pending().get()>
                            <span class="auth-submit-text">"Sign in"</span>
                            <span class="auth-key">"↵"</span>
                        </button>
                    </ActionForm>
                    {problem(action)}
                </div>
            </div>
        </main>
    }
}

/// The two rules, stated where the password is chosen. The server checks them
/// again and is the one that decides.
#[component]
fn PasswordRules() -> impl IntoView {
    view! {
        <ul class="auth-rules">
            <li>"at least 10 characters"</li>
            <li>"not your address or your name"</li>
        </ul>
    }
}

/// Whatever the server refused with, in its own words — from the action when
/// the page has script, and from the address it was redirected to when it does
/// not.
fn problem<S>(action: ServerAction<S>) -> impl IntoView
where
    S: leptos::server_fn::ServerFn<Output = Option<Refusal>> + Send + Sync + Clone + 'static,
    S::Error: Clone + Send + Sync + 'static,
{
    let refusal = crate::auth::refusal_of(action);
    let message = move || refusal().map(|refusal| refusal.message());
    view! {
        <Show when=move || message().is_some()>
            <div class="auth-problem">{move || message().unwrap_or_default()}</div>
        </Show>
    }
}
