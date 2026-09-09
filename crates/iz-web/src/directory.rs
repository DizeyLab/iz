//! How the identity mirror stands against im — the facts the Settings
//! Connection card renders. The mirror task writes; the page reads.
//!
//! Deliberately dumb: one mutex over four plain fields, because the card
//! renders once per view and the stream updates a few times a minute. The
//! client id and issuer are public by OIDC design; the client secret never
//! comes near this struct, so the render path cannot leak what it was never
//! given.

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use topcoat::context::{Cx, try_app_context};

/// The mirror's health, shared between the stream task and the renderer.
#[derive(Clone, Default)]
pub struct DirectoryHealth(Arc<Mutex<Inner>>);

#[derive(Default)]
struct Inner {
    /// The `/directory/live` stream is open.
    connected: bool,
    opened_at: Option<Instant>,
    event_at: Option<Instant>,
    pass_at: Option<Instant>,
}

/// One rendered view of [`DirectoryHealth`]: the state word the card shows
/// and the ages behind the two quiet facts.
#[derive(Clone, Copy)]
pub struct Snapshot {
    pub connected: bool,
    pub event_age: Option<Duration>,
    pub pass_age: Option<Duration>,
}

impl DirectoryHealth {
    pub fn new() -> Self {
        Self::default()
    }

    /// The stream opened (or reopened): the card may say Connected.
    pub fn connected(&self) {
        let mut inner = self.0.lock();
        inner.connected = true;
        inner.opened_at = Some(Instant::now());
    }

    /// The stream ended or im refused it: the card says Reconnecting until
    /// the next full pass opens a fresh one.
    pub fn reconnecting(&self) {
        self.0.lock().connected = false;
    }

    /// A Profile event came off the stream.
    pub fn event(&self) {
        self.0.lock().event_at = Some(Instant::now());
    }

    /// A full roster pass finished — the stream's boot replay, a resync
    /// after a drop, or the watchdog beat's pass.
    pub fn pass(&self) {
        self.0.lock().pass_at = Some(Instant::now());
    }

    pub fn snapshot(&self) -> Snapshot {
        let inner = self.0.lock();
        let now = Instant::now();
        Snapshot {
            connected: inner.connected,
            event_age: inner.event_at.map(|at| now.saturating_duration_since(at)),
            pass_age: inner.pass_at.map(|at| now.saturating_duration_since(at)),
        }
    }
}

/// The health `main.rs` registers on the router beside the directory
/// client it describes.
pub fn health(cx: &Cx) -> &DirectoryHealth {
    try_app_context::<DirectoryHealth>(cx)
        .expect("the directory health was registered on the router")
}

/// The stream's age in whole seconds, as the card prints it — a short
/// number with its unit, the same shape in every language.
pub fn age_text(age: Option<Duration>) -> String {
    match age {
        Some(age) => {
            let secs = age.as_secs();
            if secs >= 60 {
                format!("{}m", secs / 60)
            } else {
                format!("{secs}s")
            }
        }
        None => String::new(),
    }
}
