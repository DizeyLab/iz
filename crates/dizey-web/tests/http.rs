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
        let dir = std::env::temp_dir().join(format!("dizey-http-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = TursoStore::open(dir.join("dizey.db").to_str().unwrap())
            .await
            .unwrap();
        let options = LeptosOptions::builder().output_name("dizey").build();
        let router = dizey_web::server::router(Accounts::new(Arc::new(store)), options);
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
    let viewer = invited(
        &app,
        &admin,
        "quiet@dizey.sh",
        "Quiet Reader",
        Role::Viewer,
    )
    .await;
    let column = first_column(&app, &admin).await;

    let answer = app
        .post(
            path::<dizey_web::board::CreateTask>(),
            Some(&viewer),
            &[("title", "Viewer should not get this"), ("column_id", &column)],
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
