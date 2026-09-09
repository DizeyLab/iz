//! How attachments reach the family's Files service, and how the Settings
//! card sees that service: the storage client and its health, one file.
//!
//! The client is stateless by design — the base URL is a per-call argument,
//! because the family can move between beats and a URL captured at boot is
//! a URL that goes stale. The token is the one thing held: it comes from
//! `config/iz.toml` at boot and never leaves this struct except onto the
//! wire, as a Bearer header. It is never logged and never rendered.
//!
//! Every call carries a ceiling so a stalled service cannot park a request
//! or the drain forever: a connect ceiling on the client, a read ceiling
//! between body bytes, and a total ceiling on the small calls. A large push
//! or fetch takes as long as the bytes take — the ceilings bite silence,
//! not size.

use std::sync::Arc;
use std::time::{Duration, Instant};

use iz_core::store::Store;
use parking_lot::Mutex;
use topcoat::context::{Cx, try_app_context};

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Why a storage call did not do what it was asked. `Quota` is in's own word
/// for "the limit im set is spent"; everything else is either the wire or a
/// refusal whose words only a log can use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageProblem {
    /// The service did not answer at all — down, mid-deploy, unreachable.
    Unreachable,
    /// in refused the push because the account's bytes are spent.
    Quota,
    /// The service answered with something unusable. The words are for the
    /// log and the card's "last problem", never for a page.
    Other(String),
}

/// What in reports about iz's service account: the limit im set, and how
/// much of it the stored attachments hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
pub struct Status {
    #[serde(default)]
    pub quota_bytes: u64,
    #[serde(default)]
    pub used_bytes: u64,
}

#[derive(Clone)]
pub struct StorageClient {
    http: reqwest::Client,
    token: Option<String>,
}

impl StorageClient {
    /// One client per process: it holds a connection pool. A `None` token is
    /// a deployment with no `[storage.in]` — the client exists (the handlers
    /// keep one shape) and every call refuses before touching the wire.
    pub fn new(token: Option<String>) -> Self {
        let http = reqwest::Client::builder()
            // A stalled connect is the one hang a total ceiling on the small
            // calls cannot cover; five seconds is generous for a LAN peer.
            .connect_timeout(Duration::from_secs(5))
            // Silence between body bytes, not total time: a half-sent 400 MB
            // attachment is a transfer, a half-sent nothing is a stall.
            .read_timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        Self { http, token }
    }

    fn bearer(&self) -> Option<&str> {
        self.token.as_deref()
    }

    /// Is the service standing? `GET /healthz` is open — no key, no body to
    /// read — because a probe that needed the account could not tell "in is
    /// down" from "the key was rotated" without a second call.
    pub async fn probe(&self, url: &str) -> bool {
        let Ok(reply) = self
            .http
            .get(format!("{url}/healthz"))
            .timeout(Duration::from_secs(10))
            .send()
            .await
        else {
            return false;
        };
        reply.status().is_success()
    }

    /// The account's bytes: the limit im set and what is held. `None` on a
    /// refused key, a service too old to know the route, or a dead wire —
    /// the card reads it as "no facts yet", the same quiet it starts with.
    pub async fn status(&self, url: &str) -> Option<Status> {
        let reply = self
            .http
            .get(format!("{url}/api/service/status"))
            .bearer_auth(self.bearer()?)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .ok()?;
        if !reply.status().is_success() {
            return None;
        }
        reply.json::<Status>().await.ok()
    }

    /// Pushes one attachment. Idempotent by construction: in keys its rows
    /// by `external_id`, which is iz's own attachment id, so a retry after
    /// an unclear answer replaces the same bytes rather than doubling them.
    /// Answers in's file id — the same `external_id` it was handed.
    pub async fn put(
        &self,
        url: &str,
        external_id: &str,
        name: &str,
        bytes: Vec<u8>,
        mime: &str,
    ) -> Result<String, StorageProblem> {
        let token = self.bearer().ok_or(StorageProblem::Other("no key".into()))?;
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(name.to_string())
            .mime_str(mime)
            .map_err(|_| StorageProblem::Other(format!("mime {mime:?}")))?;
        let form = reqwest::multipart::Form::new()
            .text("external_id", external_id.to_string())
            .text("name", name.to_string())
            .part("file", part);
        // No total ceiling on purpose: the widest attachment iz allows is
        // the widest push this makes, and the read ceiling above is what
        // stops a stalled body.
        let reply = self
            .http
            .post(format!("{url}/api/service/files"))
            .bearer_auth(token)
            .multipart(form)
            .send()
            .await
            .map_err(|_| StorageProblem::Unreachable)?;
        let body: serde_json::Value = reply
            .json()
            .await
            .map_err(|problem| StorageProblem::Other(problem.to_string()))?;
        if let Some(id) = body.get("ok").and_then(|id| id.as_str()) {
            return Ok(id.to_string());
        }
        match body.get("err").and_then(|err| err.as_str()) {
            Some("QuotaExceeded") => Err(StorageProblem::Quota),
            Some(other) => Err(StorageProblem::Other(other.to_string())),
            None => Err(StorageProblem::Other("unusable answer".into())),
        }
    }

    /// Fetches one attachment's bytes back. The row exists in iz — an
    /// unreachable service is a problem to say so, never a not-found.
    pub async fn fetch(&self, url: &str, external_id: &str) -> Result<Vec<u8>, StorageProblem> {
        let token = self.bearer().ok_or(StorageProblem::Other("no key".into()))?;
        let reply = self
            .http
            .get(format!("{url}/api/service/file/{external_id}"))
            .bearer_auth(token)
            .send()
            .await
            .map_err(|_| StorageProblem::Unreachable)?;
        if !reply.status().is_success() {
            return Err(StorageProblem::Other(format!(
                "fetch answered {}",
                reply.status()
            )));
        }
        let bytes = reply
            .bytes()
            .await
            .map_err(|_| StorageProblem::Unreachable)?;
        Ok(bytes.to_vec())
    }

    /// Takes one attachment away from in — a hard delete, no trash, the
    /// same hard delete iz's own store does.
    pub async fn delete(&self, url: &str, external_id: &str) -> Result<(), StorageProblem> {
        let token = self.bearer().ok_or(StorageProblem::Other("no key".into()))?;
        let reply = self
            .http
            .delete(format!("{url}/api/service/file/{external_id}"))
            .bearer_auth(token)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|_| StorageProblem::Unreachable)?;
        if reply.status().is_success() {
            Ok(())
        } else {
            Err(StorageProblem::Other(format!(
                "delete answered {}",
                reply.status()
            )))
        }
    }
}

/// The client `main.rs` registers beside the store; every handler that
/// branches on the backend reaches it through here.
pub fn client(cx: &Cx) -> &StorageClient {
    try_app_context::<StorageClient>(cx)
        .expect("the storage client was registered on the router")
}

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

/// The Files service's stand, as the Settings card renders it. The storage
/// beat writes; the page reads.
///
/// Deliberately dumb, like [`crate::directory::DirectoryHealth`] beside
/// which it lives: one mutex over plain fields, written a few times a
/// minute, read once a view. The token never comes near this struct, so the
/// render path cannot leak what it was never given.
#[derive(Clone, Default)]
pub struct StorageHealth(Arc<Mutex<Inner>>);

#[derive(Default)]
struct Inner {
    /// Where the family says in lives, and what im named it — refreshed
    /// every beat, so a moved service is followed, not remembered.
    url: Option<String>,
    name: Option<String>,
    /// The last probe found a standing service.
    connected: bool,
    used: Option<u64>,
    quota: Option<u64>,
    /// The drain's counter: `(moved so far, total there were to move)`.
    /// `None` whenever no drain is running — before the first, and the
    /// moment the last row moved.
    migrating: Option<(u64, u64)>,
    last_ok: Option<Instant>,
    /// What went wrong, as a short machine word, and when.
    last_problem: Option<(Instant, String)>,
}

/// One rendered view of [`StorageHealth`].
#[derive(Clone)]
pub struct StorageSnapshot {
    pub has_service: bool,
    pub connected: bool,
    pub used: Option<u64>,
    pub quota: Option<u64>,
    pub migrating: Option<(u64, u64)>,
    pub ok_age: Option<Duration>,
    pub problem_age: Option<Duration>,
    pub problem_word: Option<String>,
}

impl StorageHealth {
    pub fn new() -> Self {
        Self::default()
    }

    /// The family mirror named the service this beat.
    pub fn seen(&self, url: &str, name: &str) {
        let mut inner = self.0.lock();
        inner.url = Some(url.to_string());
        inner.name = Some(name.to_string());
    }

    /// The family mirror has no row for in.
    pub fn unlisted(&self) {
        let mut inner = self.0.lock();
        inner.url = None;
        inner.name = None;
        inner.connected = false;
    }

    /// The probe and the status both landed: the facts are current.
    pub fn reachable(&self, used: Option<u64>, quota: Option<u64>) {
        let mut inner = self.0.lock();
        inner.connected = true;
        inner.used = used;
        inner.quota = quota;
        inner.last_ok = Some(Instant::now());
    }

    /// The probe or the status did not land. The word is the fact in
    /// shorthand — `unreachable`, `quota` — for the card's one line.
    pub fn problem(&self, word: &str) {
        let mut inner = self.0.lock();
        inner.connected = false;
        inner.last_problem = Some((Instant::now(), word.to_string()));
    }

    /// A drain is running and moved one more row.
    pub fn migrating(&self, moved: u64, total: u64) {
        self.0.lock().migrating = Some((moved, total));
    }

    /// The drain finished: the counter goes, and does not come back until a
    /// new batch of local rows earns a new total.
    pub fn drained(&self) {
        self.0.lock().migrating = None;
    }

    pub fn snapshot(&self) -> StorageSnapshot {
        let inner = self.0.lock();
        let now = Instant::now();
        StorageSnapshot {
            has_service: inner.url.is_some(),
            connected: inner.connected,
            used: inner.used,
            quota: inner.quota,
            migrating: inner.migrating,
            ok_age: inner.last_ok.map(|at| now.saturating_duration_since(at)),
            problem_age: inner
                .last_problem
                .as_ref()
                .map(|(at, _)| now.saturating_duration_since(*at)),
            problem_word: inner.last_problem.as_ref().map(|(_, word)| word.clone()),
        }
    }
}

/// The health `main.rs` registers on the router beside the client it
/// describes.
pub fn health(cx: &Cx) -> &StorageHealth {
    try_app_context::<StorageHealth>(cx).expect("the storage health was registered on the router")
}

// ---------------------------------------------------------------------------
// The cycle
// ---------------------------------------------------------------------------

/// How often the storage beat runs. Short enough that an enablement starts
/// moving rows within half a minute, long enough that the probe is a
/// glance, not a conversation.
pub const STORAGE_SECONDS: u64 = 30;

/// How many rows one drain cycle pushes. The push is the expensive half, so
/// the batch is small: the beat is the retry, and four rows a beat drains a
/// real backlog quietly instead of monopolising the store.
const DRAIN_BATCH: u64 = 4;

/// Where in sits in the family mirror: its human name and its URL, or
/// nothing when the mirror has no row for it. Read per use — the family can
/// move between beats.
pub async fn in_of(store: &dyn Store) -> Option<(String, String)> {
    let raw = store.get_setting(crate::server::FAMILY_KEY).await.ok()??;
    let family: Vec<iz_client::FamilyService> = serde_json::from_str(&raw).ok()?;
    let row = family.into_iter().find(|service| service.key == "in")?;
    Some((row.name, row.url))
}

/// One pass of the storage beat: follow the family mirror, ask the service
/// how it stands, and — while the workspace's attachments live on in —
/// drain the rows still on this disk. The body is extracted from the loop
/// in `main.rs` the way `mirror_directory` is, so a test can run one beat
/// by hand.
pub async fn storage_cycle(
    store: &std::sync::Arc<dyn Store>,
    client: &StorageClient,
    health: &StorageHealth,
) {
    // 1. Where does the family say in is, this beat?
    let service = in_of(store.as_ref()).await;
    match &service {
        Some((name, url)) => health.seen(url, name),
        None => health.unlisted(),
    }
    let Some((_, url)) = service else {
        return;
    };

    // 2. How does the service stand?
    if !client.probe(&url).await {
        health.problem("unreachable");
        return;
    }
    match client.status(&url).await {
        Some(status) => health.reachable(Some(status.used_bytes), Some(status.quota_bytes)),
        None => {
            health.problem("status");
            return;
        }
    }

    // 3. Only a workspace switched to in drains; local mode leaves the
    //    local rows exactly where they are.
    if store
        .storage_backend()
        .await
        .unwrap_or(iz_core::store::StorageBackend::Local)
        != iz_core::store::StorageBackend::In
    {
        return;
    }
    let count = store.local_attachment_count().await.unwrap_or(0);
    if count == 0 {
        return;
    }

    // The total is captured when a drain starts and never re-captured
    // while one runs, so the counter only ever climbs to its end. A beat
    // that finds the counter standing picks up where it left off.
    let total = match health.snapshot().migrating {
        Some((_, total)) => total,
        None => count,
    };

    let Ok(rows) = store.local_attachments(DRAIN_BATCH).await else {
        health.problem("store");
        return;
    };
    for row in rows {
        let Ok(Some(bytes)) = store.attachment_bytes(&row.id).await else {
            health.problem("store");
            return;
        };
        match client
            .put(&url, &row.id, &row.file_name, bytes, &row.mime_type)
            .await
        {
            Ok(_) => {}
            Err(StorageProblem::Quota) => {
                health.problem("quota");
                return;
            }
            Err(problem) => {
                health.problem(&problem_word(&problem));
                return;
            }
        }
        // The row flips and the file leaves this disk in one call; a push
        // that landed but a row that refuses is retried whole next beat —
        // in's push is idempotent by external id, so the replace is free.
        if store.mark_attachment_stored(&row.id).await.ok() != Some(true) {
            health.problem("store");
            return;
        }
        let remaining = store.local_attachment_count().await.unwrap_or(0);
        health.migrating(total.saturating_sub(remaining), total);
    }

    // 4. The last row of the batch gone and nothing left: the counter the
    //    card shows is gone with it, and every open admin page hears once.
    if store.local_attachment_count().await.unwrap_or(0) == 0 {
        health.drained();
        let _ = store.announce_storage().await;
    }
}

/// A storage problem as the card's one word for it.
fn problem_word(problem: &StorageProblem) -> String {
    match problem {
        StorageProblem::Unreachable => "unreachable".to_string(),
        StorageProblem::Quota => "quota".to_string(),
        StorageProblem::Other(other) => other.chars().take(40).collect(),
    }
}

/// Bytes in human units: `512 B`, `1.5 KiB`, `2.0 GiB`. One decimal past
/// bytes so a quota line reads at a glance. Ported from in's settings panel,
/// whose usage lines this card's Limit row mirrors — the family renders
/// bytes one way.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
