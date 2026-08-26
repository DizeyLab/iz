//! The app driven the way a browser drives it: real router, real handlers, real
//! session cookie.
//!
//! The point of this binary is the guards. A button the UI does not draw proves
//! nothing — a Viewer who posts to the mutation endpoint anyway must be refused
//! by the handler, and that is what these tests call.
//!
//! New HTTP tests belong in this file rather than a new `tests/*.rs`: one test
//! binary links and runs once.
#![cfg(feature = "ssr")]

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderValue, Request, StatusCode, header};
use dizey_core::Role;
use dizey_core::accounts::Accounts;
use dizey_core::store::TursoStore;
use dizey_web::server::SESSION_COOKIE;
use leptos::prelude::LeptosOptions;
use leptos::server_fn::ServerFn;
use tower::ServiceExt;
use uuid::Uuid;

/// A throwaway workspace: its own database file and its own router.
struct App {
    dir: PathBuf,
    router: Router,
}

impl App {
    async fn open() -> Self {
        Self::open_with(None).await
    }

    /// The same app, started the way a deployment with a sender configured
    /// starts it. The password is not among what the router is given.
    async fn with_sender() -> Self {
        Self::open_with(Some(dizey_web::settings::Sender {
            host: "smtp.fastmail.com".to_string(),
            port: 465,
            username: "dizey".to_string(),
            from: "dizey@dizey.sh".to_string(),
        }))
        .await
    }

    async fn open_with(sender: Option<dizey_web::settings::Sender>) -> Self {
        let dir = std::env::temp_dir().join(format!("dizey-http-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = TursoStore::open(dir.join("dizey.db").to_str().unwrap())
            .await
            .unwrap();
        let options = LeptosOptions::builder().output_name("dizey").build();
        let router = dizey_web::server::router(
            Accounts::new(Arc::new(store)),
            dizey_web::server::Mail::silent(),
            sender,
            options,
        );
        Self { dir, router }
    }

    /// Posts a form to a server function, as the browser does, and returns the
    /// status, the JSON body and any session cookie the answer set.
    async fn post(&self, path: &str, cookie: Option<&str>, form: &[(&str, &str)]) -> Answer {
        let body = form
            .iter()
            .map(|(key, value)| format!("{}={}", encode(key), encode(value)))
            .collect::<Vec<_>>()
            .join("&");
        let mut request = Request::builder()
            .method("POST")
            .uri(path)
            .header(header::ACCEPT, "application/json")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(cookie) = cookie {
            request = request.header(
                header::COOKIE,
                HeaderValue::from_str(&format!("{SESSION_COOKIE}={cookie}")).unwrap(),
            );
        }
        let response = self
            .router
            .clone()
            .oneshot(request.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let session = response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .find_map(session_from);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        Answer {
            status,
            session,
            body: String::from_utf8(bytes.to_vec()).unwrap(),
            location,
        }
    }

    /// Posts the same form the way a browser with no script posts it: asking
    /// for a page back, from a page it names. The answer is a redirect, and the
    /// redirect is the whole of what the person will be shown.
    async fn post_without_script(
        &self,
        path: &str,
        cookie: Option<&str>,
        referer: &str,
        form: &[(&str, &str)],
    ) -> Answer {
        let body = form
            .iter()
            .map(|(key, value)| format!("{}={}", encode(key), encode(value)))
            .collect::<Vec<_>>()
            .join("&");
        let mut request = Request::builder()
            .method("POST")
            .uri(path)
            .header(header::ACCEPT, "text/html")
            .header(header::REFERER, referer)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(cookie) = cookie {
            request = request.header(
                header::COOKIE,
                HeaderValue::from_str(&format!("{SESSION_COOKIE}={cookie}")).unwrap(),
            );
        }
        let response = self
            .router
            .clone()
            .oneshot(request.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        Answer {
            status,
            session: None,
            body: String::from_utf8(bytes.to_vec()).unwrap(),
            location,
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Answer {
    status: StatusCode,
    session: Option<String>,
    body: String,
    /// Where a browser without script is sent next. The only place such a
    /// browser can be told anything.
    location: Option<String>,
}

/// The `Set-Cookie` value's session token, if that header carries a live one.
fn session_from(raw: &str) -> Option<String> {
    let value = raw.strip_prefix(SESSION_COOKIE)?.strip_prefix('=')?;
    let value = value.split(';').next()?.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// Form encoding, enough for the addresses and passphrases these tests send.
fn encode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

fn path<F: ServerFn>() -> &'static str {
    F::PATH
}

/// Claims the workspace and returns the admin's session cookie.
async fn admin(app: &App) -> String {
    let answer = app
        .post(
            path::<dizey_web::auth::ClaimWorkspace>(),
            None,
            &[
                ("display_name", "Ada Lovelace"),
                ("email", "ada@dizey.sh"),
                ("password", "correct horse battery staple"),
            ],
        )
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert_eq!(answer.body, "null", "claiming was refused");
    answer.session.expect("claiming set no session cookie")
}

/// Invites someone in the given role and signs them in, returning their cookie.
async fn invited(app: &App, admin: &str, email: &str, name: &str, role: Role) -> String {
    let role = match role {
        Role::Admin => "admin",
        Role::Member => "member",
        Role::Viewer => "viewer",
    };
    let answer = app
        .post(
            path::<dizey_web::auth::InviteMember>(),
            Some(admin),
            &[("email", email), ("display_name", name), ("role", role)],
        )
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    let token = answer
        .body
        .rsplit_once("/join/")
        .and_then(|(_, rest)| rest.split('"').next())
        .expect("no invitation link in {answer.body}")
        .to_string();

    let answer = app
        .post(
            path::<dizey_web::auth::RedeemLink>(),
            None,
            &[
                ("token", &token),
                ("password", "lantern gravel spoon meadow"),
            ],
        )
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert_eq!(answer.body, "null", "first sign-in was refused");
    answer.session.expect("first sign-in set no session cookie")
}

/// The id of the board's first column, read the way the board page reads it.
async fn first_column(app: &App, cookie: &str) -> String {
    let answer = app
        .post(path::<dizey_web::board::CurrentBoard>(), Some(cookie), &[])
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    answer
        .body
        .split_once("\"columns\":[{\"column\":{\"id\":\"")
        .and_then(|(_, rest)| rest.split('"').next())
        .expect("no column in the board")
        .to_string()
}

#[tokio::test]
async fn a_viewer_who_posts_to_create_task_anyway_is_refused() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let viewer = invited(&app, &admin, "quiet@dizey.sh", "Quiet Reader", Role::Viewer).await;
    let column = first_column(&app, &admin).await;

    let answer = app
        .post(
            path::<dizey_web::board::CreateTask>(),
            Some(&viewer),
            &[
                ("title", "Viewer should not get this"),
                ("column_id", &column),
            ],
        )
        .await;

    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert_eq!(answer.body, "\"Forbidden\"");

    // And the refusal is not cosmetic: the board is still empty.
    let answer = app
        .post(path::<dizey_web::board::CurrentBoard>(), Some(&admin), &[])
        .await;
    assert!(
        !answer.body.contains("Viewer should not get this"),
        "the refused task was written anyway: {}",
        answer.body
    );
}

#[tokio::test]
async fn a_member_may_create_a_task() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let member = invited(&app, &admin, "mo@dizey.sh", "Mo Dubois", Role::Member).await;
    let column = first_column(&app, &admin).await;

    let answer = app
        .post(
            path::<dizey_web::board::CreateTask>(),
            Some(&member),
            &[("title", "Wire the deadline chip"), ("column_id", &column)],
        )
        .await;
    assert_eq!(answer.body, "null", "a member was refused: {}", answer.body);

    let answer = app
        .post(path::<dizey_web::board::CurrentBoard>(), Some(&admin), &[])
        .await;
    assert!(answer.body.contains("Wire the deadline chip"));
    assert!(answer.body.contains("DZ-01"), "no key on the new task");
}

#[tokio::test]
async fn a_task_cannot_be_dropped_into_another_workspaces_column() {
    let app = App::open().await;
    let admin = admin(&app).await;

    let answer = app
        .post(
            path::<dizey_web::board::CreateTask>(),
            Some(&admin),
            &[
                ("title", "Wrong column"),
                ("column_id", "00000000-0000-0000-0000-000000000000"),
            ],
        )
        .await;
    assert_eq!(answer.body, "\"Forbidden\"");
}

#[tokio::test]
async fn a_card_needs_a_title() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let column = first_column(&app, &admin).await;

    let answer = app
        .post(
            path::<dizey_web::board::CreateTask>(),
            Some(&admin),
            &[("title", "   "), ("column_id", &column)],
        )
        .await;
    assert_eq!(answer.body, "\"EmptyTitle\"");
}

#[tokio::test]
async fn the_board_is_not_readable_without_a_session() {
    let app = App::open().await;
    let _admin = admin(&app).await;

    let answer = app
        .post(path::<dizey_web::board::CurrentBoard>(), None, &[])
        .await;
    assert_eq!(answer.body, "{\"Err\":\"SignInFirst\"}");
}

/// Makes a task and hands back its id, read off the board the way the browser
/// would.
async fn a_task(app: &App, cookie: &str, column: &str, title: &str) -> String {
    let answer = app
        .post(
            path::<dizey_web::board::CreateTask>(),
            Some(cookie),
            &[("title", title), ("column_id", column)],
        )
        .await;
    assert_eq!(answer.body, "null", "the task was refused: {}", answer.body);

    let answer = app
        .post(path::<dizey_web::board::CurrentBoard>(), Some(cookie), &[])
        .await;
    let needle = format!("\"title\":\"{title}\"");
    let before = answer
        .body
        .split_once(&needle)
        .map(|(head, _)| head)
        .expect("the new task is not on the board");
    before
        .rsplit_once("{\"id\":\"")
        .and_then(|(_, rest)| rest.split('"').next())
        .expect("no id on the new task")
        .to_string()
}

#[tokio::test]
async fn a_viewer_who_posts_a_comment_anyway_is_refused() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let viewer = invited(&app, &admin, "eyes@dizey.sh", "Ida Eyes", Role::Viewer).await;
    let column = first_column(&app, &admin).await;
    let task = a_task(&app, &admin, &column, "Ship the detail modal").await;

    let answer = app
        .post(
            path::<dizey_web::detail::PostComment>(),
            Some(&viewer),
            &[("task_id", &task), ("body", "Viewers cannot say this")],
        )
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert_eq!(answer.body, "\"Forbidden\"");

    // The refusal is not cosmetic: nothing was written.
    let answer = app
        .post(
            path::<dizey_web::detail::FetchTask>(),
            Some(&admin),
            &[("task_id", &task)],
        )
        .await;
    assert!(
        !answer.body.contains("Viewers cannot say this"),
        "the refused comment was written anyway: {}",
        answer.body
    );
}

#[tokio::test]
async fn a_member_may_comment_and_the_author_is_the_session() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let member = invited(&app, &admin, "kai@dizey.sh", "Kai Renner", Role::Member).await;
    let column = first_column(&app, &admin).await;
    let task = a_task(&app, &admin, &column, "Wire the picker").await;

    let answer = app
        .post(
            path::<dizey_web::detail::PostComment>(),
            Some(&member),
            &[("task_id", &task), ("body", "Picker is narrow on purpose")],
        )
        .await;
    assert_eq!(answer.body, "null", "a member was refused: {}", answer.body);

    let answer = app
        .post(
            path::<dizey_web::detail::FetchTask>(),
            Some(&admin),
            &[("task_id", &task)],
        )
        .await;
    assert!(answer.body.contains("Picker is narrow on purpose"));
    assert!(
        answer.body.contains("Kai Renner"),
        "the comment is not attributed to the session's user: {}",
        answer.body
    );
}

#[tokio::test]
async fn a_link_that_would_close_a_circle_is_refused_at_the_endpoint() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let column = first_column(&app, &admin).await;
    let first = a_task(&app, &admin, &column, "Lay the cable").await;
    let second = a_task(&app, &admin, &column, "Light the cable").await;

    let answer = app
        .post(
            path::<dizey_web::detail::LinkTasks>(),
            Some(&admin),
            &[
                ("task_id", &second),
                ("other_id", &first),
                ("direction", "blocked_by"),
            ],
        )
        .await;
    assert_eq!(
        answer.body, "null",
        "the first link was refused: {}",
        answer.body
    );

    let answer = app
        .post(
            path::<dizey_web::detail::LinkTasks>(),
            Some(&admin),
            &[
                ("task_id", &first),
                ("other_id", &second),
                ("direction", "blocked_by"),
            ],
        )
        .await;
    assert_eq!(answer.body, "\"Cycle\"");
}

#[tokio::test]
async fn a_refusal_reaches_a_browser_with_no_script() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let column = first_column(&app, &admin).await;
    let first = a_task(&app, &admin, &column, "Lay the cable").await;
    let second = a_task(&app, &admin, &column, "Light the cable").await;

    app.post(
        path::<dizey_web::detail::LinkTasks>(),
        Some(&admin),
        &[
            ("task_id", &second),
            ("other_id", &first),
            ("direction", "blocked_by"),
        ],
    )
    .await;

    // The same link back the other way, asked for by a browser that has no way
    // to read the answer's body.
    let answer = app
        .post_without_script(
            path::<dizey_web::detail::LinkTasks>(),
            Some(&admin),
            &format!("http://dizey.test/?task={first}"),
            &[
                ("task_id", &first),
                ("other_id", &second),
                ("direction", "blocked_by"),
            ],
        )
        .await;
    assert_eq!(answer.status, StatusCode::FOUND);
    let location = answer.location.expect("a redirect with nowhere to go");
    assert!(
        location.contains("refusal=cycle") && location.contains("on=link_tasks"),
        "the refusal did not ride back on the redirect: {location}"
    );
    assert!(
        location.contains(&format!("task={first}")),
        "the modal was closed on the way back: {location}"
    );
}

#[tokio::test]
async fn a_call_that_was_not_refused_carries_nothing_back() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let column = first_column(&app, &admin).await;
    let first = a_task(&app, &admin, &column, "Lay the cable").await;
    let second = a_task(&app, &admin, &column, "Light the cable").await;

    let answer = app
        .post_without_script(
            path::<dizey_web::detail::LinkTasks>(),
            Some(&admin),
            "http://dizey.test/",
            &[
                ("task_id", &second),
                ("other_id", &first),
                ("direction", "blocked_by"),
            ],
        )
        .await;
    assert_eq!(answer.status, StatusCode::FOUND);
    let location = answer.location.expect("a redirect with nowhere to go");
    assert!(
        !location.contains("refusal="),
        "a link that was made said it was refused: {location}"
    );
}

#[tokio::test]
async fn a_task_id_from_nowhere_is_not_found() {
    let app = App::open().await;
    let admin = admin(&app).await;

    let answer = app
        .post(
            path::<dizey_web::detail::FetchTask>(),
            Some(&admin),
            &[("task_id", "00000000-0000-0000-0000-000000000000")],
        )
        .await;
    assert_eq!(answer.body, "{\"Err\":\"NotFound\"}");
}

#[tokio::test]
async fn a_viewer_who_posts_a_delete_anyway_is_refused() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let viewer = invited(&app, &admin, "wren@dizey.sh", "Wren Ash", Role::Viewer).await;
    let column = first_column(&app, &admin).await;
    let task = a_task(&app, &admin, &column, "Viewers cannot remove this").await;

    let answer = app
        .post(
            path::<dizey_web::detail::DeleteTask>(),
            Some(&viewer),
            &[("task_id", &task)],
        )
        .await;
    assert_eq!(answer.body, "\"Forbidden\"");

    let answer = app
        .post(path::<dizey_web::board::CurrentBoard>(), Some(&admin), &[])
        .await;
    assert!(
        answer.body.contains("Viewers cannot remove this"),
        "the refused delete happened anyway: {}",
        answer.body
    );
}

#[tokio::test]
async fn a_member_may_delete_and_the_delete_is_soft() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let member = invited(&app, &admin, "rae@dizey.sh", "Rae Okonkwo", Role::Member).await;
    let column = first_column(&app, &admin).await;
    let task = a_task(&app, &admin, &column, "Mistyped in a hurry").await;

    // What it would cost is a read: it says so and writes nothing.
    let answer = app
        .post(
            path::<dizey_web::detail::WhatDeleteCosts>(),
            Some(&member),
            &[("task_id", &task)],
        )
        .await;
    assert!(
        answer.body.contains("Mistyped in a hurry"),
        "{}",
        answer.body
    );
    let answer = app
        .post(path::<dizey_web::board::CurrentBoard>(), Some(&admin), &[])
        .await;
    assert!(
        answer.body.contains("Mistyped in a hurry"),
        "asking cost deleted it"
    );

    let answer = app
        .post(
            path::<dizey_web::detail::DeleteTask>(),
            Some(&member),
            &[("task_id", &task)],
        )
        .await;
    assert_eq!(answer.body, "null", "a member was refused: {}", answer.body);

    // Gone from the board, and gone from the detail: soft is not visible.
    let answer = app
        .post(path::<dizey_web::board::CurrentBoard>(), Some(&admin), &[])
        .await;
    assert!(
        !answer.body.contains("Mistyped in a hurry"),
        "{}",
        answer.body
    );
    let answer = app
        .post(
            path::<dizey_web::detail::FetchTask>(),
            Some(&admin),
            &[("task_id", &task)],
        )
        .await;
    assert_eq!(answer.body, "{\"Err\":\"NotFound\"}");
}

/// Every column id on the board, in order, read off the wire the way the
/// browser would.
async fn columns_of(app: &App, cookie: &str) -> Vec<String> {
    let answer = app
        .post(path::<dizey_web::board::CurrentBoard>(), Some(cookie), &[])
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    answer
        .body
        .split("{\"column\":{\"id\":\"")
        .skip(1)
        .filter_map(|rest| rest.split('"').next())
        .map(str::to_string)
        .collect()
}

#[tokio::test]
async fn a_viewer_who_posts_a_move_anyway_is_refused() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let viewer = invited(&app, &admin, "quiet@dizey.sh", "Quiet Reader", Role::Viewer).await;
    let columns = columns_of(&app, &admin).await;
    let task = a_task(&app, &admin, &columns[0], "Stays in Backlog").await;

    let answer = app
        .post(
            path::<dizey_web::board::MoveCard>(),
            Some(&viewer),
            &[
                ("task_id", &task),
                ("from_column_id", &columns[0]),
                ("to_column_id", &columns[1]),
            ],
        )
        .await;

    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert_eq!(answer.body, "\"Forbidden\"");

    // And nothing moved: the refusal is in the handler, not in the drawing.
    let answer = app
        .post(
            path::<dizey_web::detail::FetchTask>(),
            Some(&admin),
            &[("task_id", &task)],
        )
        .await;
    assert!(
        answer.body.contains(&columns[0]),
        "the refused move happened anyway: {}",
        answer.body
    );
}

#[tokio::test]
async fn a_member_may_move_a_card() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let member = invited(&app, &admin, "mo@dizey.sh", "Mo Dubois", Role::Member).await;
    let columns = columns_of(&app, &admin).await;
    let task = a_task(&app, &admin, &columns[0], "Gets picked up").await;

    let answer = app
        .post(
            path::<dizey_web::board::MoveCard>(),
            Some(&member),
            &[
                ("task_id", &task),
                ("from_column_id", &columns[0]),
                ("to_column_id", &columns[1]),
            ],
        )
        .await;
    assert_eq!(answer.body, "null", "a member was refused: {}", answer.body);

    let answer = app
        .post(
            path::<dizey_web::detail::FetchTask>(),
            Some(&admin),
            &[("task_id", &task)],
        )
        .await;
    assert!(answer.body.contains("\"moved\""), "no move in the activity");
}

#[tokio::test]
async fn a_drop_decided_against_a_stale_board_is_refused() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let member = invited(&app, &admin, "mo@dizey.sh", "Mo Dubois", Role::Member).await;
    let columns = columns_of(&app, &admin).await;
    let task = a_task(&app, &admin, &columns[0], "Contested").await;

    // Two people picked the same card up out of Backlog.
    let first = app
        .post(
            path::<dizey_web::board::MoveCard>(),
            Some(&admin),
            &[
                ("task_id", &task),
                ("from_column_id", &columns[0]),
                ("to_column_id", &columns[1]),
            ],
        )
        .await;
    assert_eq!(first.body, "null");

    let second = app
        .post(
            path::<dizey_web::board::MoveCard>(),
            Some(&member),
            &[
                ("task_id", &task),
                ("from_column_id", &columns[0]),
                ("to_column_id", &columns[2]),
            ],
        )
        .await;
    assert_eq!(second.body, "\"MovedAlready\"");

    // The winner's move stands, and there is exactly one crossing.
    let answer = app
        .post(
            path::<dizey_web::detail::FetchTask>(),
            Some(&admin),
            &[("task_id", &task)],
        )
        .await;
    assert_eq!(
        answer.body.matches("\"moved\"").count(),
        1,
        "the second drop wrote a crossing too: {}",
        answer.body
    );
}

#[tokio::test]
async fn a_card_cannot_be_moved_into_another_boards_column() {
    let app = App::open().await;
    let admin = admin(&app).await;
    let columns = columns_of(&app, &admin).await;
    let task = a_task(&app, &admin, &columns[0], "Stays put").await;

    let answer = app
        .post(
            path::<dizey_web::board::MoveCard>(),
            Some(&admin),
            &[
                ("task_id", &task),
                ("from_column_id", &columns[0]),
                ("to_column_id", "00000000-0000-0000-0000-000000000000"),
            ],
        )
        .await;
    assert_eq!(answer.body, "\"Forbidden\"");
}

// --- settings ---------------------------------------------------------------

#[tokio::test]
async fn a_signed_out_browser_is_told_nothing_by_the_settings_call() {
    let app = App::with_sender().await;
    let _ = admin(&app).await;

    let answer = app
        .post(path::<dizey_web::settings::CurrentSettings>(), None, &[])
        .await;

    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert_eq!(answer.body, "{\"Err\":\"SignInFirst\"}");
    assert!(!answer.body.contains("fastmail"), "{}", answer.body);
}

/// The sender panel is admin-only, and "only" is decided on the server: a
/// Member asking the same call gets an answer with no sender in it at all,
/// rather than a hidden panel whose contents rode along in the body.
#[tokio::test]
async fn a_member_is_told_nothing_about_the_sender() {
    let app = App::with_sender().await;
    let admin_cookie = admin(&app).await;
    let member = invited(
        &app,
        &admin_cookie,
        "emre@dizey.sh",
        "Emre",
        Role::Member,
    )
    .await;

    let answer = app
        .post(
            path::<dizey_web::settings::CurrentSettings>(),
            Some(&member),
            &[],
        )
        .await;

    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert!(answer.body.contains("\"administers\":false"), "{}", answer.body);
    assert!(answer.body.contains("\"sender\":null"), "{}", answer.body);
    assert!(!answer.body.contains("fastmail"), "{}", answer.body);
}

/// The admin sees what the process is configured with — and the password is
/// not part of "what": it is never held anywhere this call can reach.
#[tokio::test]
async fn an_admin_sees_the_sender_and_never_a_password() {
    let app = App::with_sender().await;
    let admin_cookie = admin(&app).await;

    let answer = app
        .post(
            path::<dizey_web::settings::CurrentSettings>(),
            Some(&admin_cookie),
            &[],
        )
        .await;

    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert!(answer.body.contains("smtp.fastmail.com"), "{}", answer.body);
    assert!(answer.body.contains("465"), "{}", answer.body);
    assert!(
        !answer.body.to_lowercase().contains("password"),
        "the answer carries a password field: {}",
        answer.body
    );
}

/// The form carries a name and nothing else. Who is renamed comes from the
/// session, so there is no id to tamper with.
#[tokio::test]
async fn saving_a_profile_renames_the_person_asking_and_nobody_else() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;
    let member = invited(&app, &admin_cookie, "emre@dizey.sh", "Emre", Role::Member).await;

    let answer = app
        .post(
            path::<dizey_web::settings::SaveProfile>(),
            Some(&member),
            &[("display_name", "Emre Y")],
        )
        .await;
    assert_eq!(answer.body, "null", "{}", answer.body);

    let mine = app
        .post(
            path::<dizey_web::settings::CurrentSettings>(),
            Some(&member),
            &[],
        )
        .await;
    assert!(mine.body.contains("Emre Y"), "{}", mine.body);

    let theirs = app
        .post(
            path::<dizey_web::settings::CurrentSettings>(),
            Some(&admin_cookie),
            &[],
        )
        .await;
    assert!(theirs.body.contains("Ada Lovelace"), "{}", theirs.body);
}

#[tokio::test]
async fn a_profile_cannot_be_saved_without_a_name() {
    let app = App::open().await;
    let admin_cookie = admin(&app).await;

    let answer = app
        .post(
            path::<dizey_web::settings::SaveProfile>(),
            Some(&admin_cookie),
            &[("display_name", "   ")],
        )
        .await;
    assert_eq!(answer.body, "\"EmptyName\"", "{}", answer.body);
}

#[tokio::test]
async fn a_signed_out_browser_cannot_rename_anybody() {
    let app = App::open().await;
    let _ = admin(&app).await;

    let answer = app
        .post(
            path::<dizey_web::settings::SaveProfile>(),
            None,
            &[("display_name", "Whoever")],
        )
        .await;
    assert_eq!(answer.body, "\"SignInFirst\"", "{}", answer.body);
}
