//! Committed-write announcements.
//!
//! A write that committed publishes one [`Change`] per surface it touches,
//! and the change carries a topic — never data. That is deliberate: a
//! channel that carried rows would need a role check per message, and one
//! forgotten filter would leak. A topic cannot leak anything; the woken
//! client re-fetches through the ordinary route, and the existing role gate
//! answers there.
//!
//! [`Change::seq`] is a process-local counter, so a client that sees 41 and
//! then 43 knows it missed one and can resync instead of rendering a mix of
//! before and after.

use std::sync::atomic::{AtomicU64, Ordering};

/// The surface a committed write touched. One write may touch several: a
/// comment is a task's detail and the activity log at once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Topic {
    /// The board: task create, move, delete, save, columns.
    Board,
    /// One task's detail: comments, subtasks, links, attachments,
    /// assignees, deadline.
    Task(String),
    /// Users, roles, invites, sessions, photos, profile.
    Members,
    /// The mail queue: `mail_send` rows.
    Queue,
    /// Mail rules and the decisions they made.
    Rules,
    /// Workspace settings, sender, limits, public URL.
    Settings,
    /// The activity and event log.
    Activity,
}

impl Topic {
    /// The name the wire uses for this topic: the SSE `event:` field, and
    /// the key the role filter decides on.
    pub fn kind(&self) -> &'static str {
        match self {
            Topic::Board => "board",
            Topic::Task(_) => "task",
            Topic::Members => "members",
            Topic::Queue => "queue",
            Topic::Rules => "rules",
            Topic::Settings => "settings",
            Topic::Activity => "activity",
        }
    }

    /// The task this topic is about, when it is about one.
    pub fn id(&self) -> Option<&str> {
        match self {
            Topic::Task(id) => Some(id),
            _ => None,
        }
    }
}

/// One committed write: which surface changed, and where this change sits
/// in the store's sequence. Everything a client needs to know to refresh —
/// and nothing about what changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub topic: Topic,
    pub seq: u64,
}

static SEQ: AtomicU64 = AtomicU64::new(0);

/// The next sequence number, starting at 1. Process-local on purpose: the
/// sequence exists to catch a dropped event inside one stream, not to order
/// two servers.
pub fn next_seq() -> u64 {
    SEQ.fetch_add(1, Ordering::Relaxed) + 1
}
