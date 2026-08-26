//! The task detail vocabulary: one task with everything hung off it — status,
//! assignees, deadline, description, both directions of its dependencies, its
//! comments and its activity trail.
//!
//! Like [`board`](crate::board), this module compiles to wasm: the browser
//! renders these exact types.

use serde::{Deserialize, Serialize};
use time::{Date, OffsetDateTime};

use crate::board::{Column, Person, day_label};

/// The stored task, plus the two ids the reader needs to know it is allowed to
/// see it. The description lives here rather than on
/// [`TaskRow`](crate::board::TaskRow) so a board sweep does not carry every
/// task's prose to the browser.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskFacts {
    pub row: crate::board::TaskRow,
    pub description: String,
    pub board_id: String,
    pub workspace_id: String,
}

/// One end of a dependency, seen from the task being looked at.
///
/// `cleared_at` is set when a person unlinked the edge, and `done_at` when the
/// blocking task finished. The row stays either way, so the history — and the
/// "you can start now" rule — has something to read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DependencyEdge {
    pub task_id: String,
    pub task_key: String,
    pub title: String,
    pub cleared_at: Option<OffsetDateTime>,
    pub done_at: Option<OffsetDateTime>,
}

impl DependencyEdge {
    pub fn is_cleared(&self) -> bool {
        self.cleared_at.is_some() || self.done_at.is_some()
    }

    /// The right-hand note on a `BLOCKED BY` row: `cleared Aug 16`, or what is
    /// still in the way.
    pub fn blocked_by_label(&self) -> String {
        match (self.cleared_at, self.done_at) {
            (Some(at), _) => format!("cleared {}", day_label(at.date())),
            (None, Some(at)) => format!("done {}", day_label(at.date())),
            (None, None) => "blocking this task".to_string(),
        }
    }

    /// The same note on a `BLOCKS` row.
    pub fn blocks_label(&self) -> String {
        if self.is_cleared() {
            "no longer waiting".to_string()
        } else {
            "waiting on this task".to_string()
        }
    }
}

/// One file hung off a task, as the detail screen prints it.
///
/// The bytes are not here and neither is a path: a chip carries the name to
/// show, the size to show beside it, and the id the download handler answers
/// on. The name is display text and nothing resolves it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileLine {
    pub id: String,
    pub name: String,
    pub size_bytes: u64,
    /// The comment this file was posted with, when it was posted with one.
    pub comment_id: Option<String>,
    pub uploaded_by: String,
}

impl FileLine {
    /// `840 KB`, `1.4 MB` — the size as a chip says it. Whole kilobytes below
    /// a megabyte, one decimal above, because a chip is not a disk usage
    /// report.
    pub fn size_label(&self) -> String {
        const KB: u64 = 1024;
        const MB: u64 = 1024 * 1024;
        if self.size_bytes >= MB {
            format!("{:.1} MB", self.size_bytes as f64 / MB as f64)
        } else if self.size_bytes >= KB {
            format!("{} KB", self.size_bytes / KB)
        } else {
            format!("{} B", self.size_bytes)
        }
    }
}

/// What deleting a task would take with it. The confirmation step says this
/// out loud before the button fires.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeletionCost {
    pub task_key: String,
    pub title: String,
    pub comment_count: u32,
    /// Links in either direction that stop applying.
    pub link_count: u32,
    /// Keys of the tasks that would have nothing in front of them afterwards.
    pub frees: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Comment {
    pub id: String,
    pub author: Person,
    pub at: OffsetDateTime,
    pub body: String,
}

/// What happened to a task. The kind is the machine's word for it; the sentence
/// the strip shows is built here rather than stored, so wording is not frozen
/// into the database.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityKind {
    Created,
    Retitled,
    Described,
    DeadlineSet,
    DeadlineCleared,
    Assigned,
    Unassigned,
    Linked,
    Unlinked,
    Moved,
    Unblocked,
    Deleted,
    Commented,
    /// A kind written by a newer version than the one reading it. The stored
    /// detail is shown as-is rather than dropping the row.
    Other(String),
}

impl ActivityKind {
    pub fn as_str(&self) -> &str {
        match self {
            ActivityKind::Created => "created",
            ActivityKind::Retitled => "retitled",
            ActivityKind::Described => "described",
            ActivityKind::DeadlineSet => "deadline_set",
            ActivityKind::DeadlineCleared => "deadline_cleared",
            ActivityKind::Assigned => "assigned",
            ActivityKind::Unassigned => "unassigned",
            ActivityKind::Linked => "linked",
            ActivityKind::Unlinked => "unlinked",
            ActivityKind::Moved => "moved",
            ActivityKind::Unblocked => "unblocked",
            ActivityKind::Deleted => "deleted",
            ActivityKind::Commented => "commented",
            ActivityKind::Other(raw) => raw,
        }
    }

    pub fn parse(raw: &str) -> Self {
        match raw {
            "created" => ActivityKind::Created,
            "retitled" => ActivityKind::Retitled,
            "described" => ActivityKind::Described,
            "deadline_set" => ActivityKind::DeadlineSet,
            "deadline_cleared" => ActivityKind::DeadlineCleared,
            "assigned" => ActivityKind::Assigned,
            "unassigned" => ActivityKind::Unassigned,
            "linked" => ActivityKind::Linked,
            "unlinked" => ActivityKind::Unlinked,
            "moved" => ActivityKind::Moved,
            "unblocked" => ActivityKind::Unblocked,
            "deleted" => ActivityKind::Deleted,
            "commented" => ActivityKind::Commented,
            other => ActivityKind::Other(other.to_string()),
        }
    }
}

/// One line of the activity strip.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActivityEntry {
    pub id: String,
    /// Absent when the system did it rather than a person — the rules engine
    /// clearing a dependency, for one.
    pub actor: Option<Person>,
    pub kind: ActivityKind,
    /// The variable half: the deadline that was set, the key that was linked.
    pub detail: String,
    pub at: OffsetDateTime,
}

impl ActivityEntry {
    /// The sentence after the name: `created this task`, `set deadline Aug 21`.
    pub fn sentence(&self) -> String {
        let detail = self.detail.trim();
        match &self.kind {
            ActivityKind::Created => "created this task".to_string(),
            ActivityKind::Retitled => "renamed this task".to_string(),
            ActivityKind::Described => "edited the description".to_string(),
            ActivityKind::DeadlineSet => format!("set deadline {detail}"),
            ActivityKind::DeadlineCleared => "removed the deadline".to_string(),
            ActivityKind::Assigned => format!("assigned {detail}"),
            ActivityKind::Unassigned => format!("unassigned {detail}"),
            ActivityKind::Linked => format!("linked {detail}"),
            ActivityKind::Unlinked => format!("unlinked {detail}"),
            ActivityKind::Moved => format!("moved {detail}"),
            ActivityKind::Unblocked => format!("unblocked this task — {detail}"),
            ActivityKind::Deleted => "deleted this task".to_string(),
            ActivityKind::Commented => "commented".to_string(),
            ActivityKind::Other(_) => detail.to_string(),
        }
    }

    /// `Aug 19 11:04`, the stamp the strip puts in front of the sentence.
    pub fn moment(&self) -> String {
        moment_label(self.at)
    }
}

/// `Aug 19 11:04`. Times are UTC, as everything stored is.
pub fn moment_label(at: OffsetDateTime) -> String {
    format!(
        "{} {:02}:{:02}",
        day_label(at.date()),
        at.hour(),
        at.minute()
    )
}

/// Everything one task detail needs, in one value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskDetail {
    pub id: String,
    pub task_key: String,
    pub title: String,
    pub description: String,
    /// The column the task is in — the artboard's STATUS field.
    pub column: Column,
    /// Every column on the board, so the status control has its options
    /// without a second call.
    pub columns: Vec<Column>,
    pub deadline: Option<Date>,
    pub done_at: Option<OffsetDateTime>,
    pub assignees: Vec<Person>,
    /// Who may be assigned: the workspace's writers. Viewers are never here —
    /// a viewer cannot be given work.
    pub assignable: Vec<Person>,
    /// Tasks that must finish before this one can start.
    pub blocked_by: Vec<DependencyEdge>,
    /// Tasks waiting on this one.
    pub blocks: Vec<DependencyEdge>,
    pub comments: Vec<Comment>,
    /// Files hung off this task, oldest first.
    pub files: Vec<FileLine>,
    pub activity: Vec<ActivityEntry>,
}

impl TaskDetail {
    pub fn is_done(&self) -> bool {
        self.done_at.is_some()
    }

    /// Blocked means something unfinished is in front of it.
    pub fn is_blocked(&self) -> bool {
        !self.is_done() && self.blocked_by.iter().any(|edge| !edge.is_cleared())
    }

    pub fn is_overdue(&self, today: Date) -> bool {
        match self.deadline {
            Some(day) => !self.is_done() && day < today,
            None => false,
        }
    }

    /// The DEADLINE field's text: `Aug 21 · overdue`, `Sep 12`, `no deadline`.
    pub fn deadline_label(&self, today: Date) -> String {
        match self.deadline {
            Some(day) if !self.is_done() && day < today => {
                format!("{} · overdue", day_label(day))
            }
            Some(day) => day_label(day),
            None => "no deadline".to_string(),
        }
    }

    /// `YYYY-MM-DD` for the date input, or an empty string.
    pub fn deadline_input(&self) -> String {
        self.deadline
            .map(|day| {
                format!(
                    "{:04}-{:02}-{:02}",
                    day.year(),
                    day.month() as u8,
                    day.day()
                )
            })
            .unwrap_or_default()
    }

    /// Who is not on this task yet — the picker's contents.
    pub fn unassigned(&self) -> impl Iterator<Item = &Person> {
        self.assignable
            .iter()
            .filter(|person| !self.assignees.iter().any(|on| on.id == person.id))
    }
}

#[cfg(feature = "server")]
pub use reads::{DetailReads, load};

#[cfg(feature = "server")]
mod reads {
    use async_trait::async_trait;

    use super::{ActivityEntry, Comment, DependencyEdge, FileLine, TaskDetail, TaskFacts};
    use crate::board::{Column, Person};
    use crate::store::Result;

    /// One task's read half, split out the way [`BoardReads`](crate::BoardReads)
    /// is, so a test can wrap it and count the round trips a detail costs.
    ///
    /// Every method here is one sweep of one thing. There is deliberately no
    /// "the person who wrote this comment" call: authors arrive joined.
    #[async_trait]
    pub trait DetailReads: Send + Sync {
        /// The task and the board it belongs to, or `None` if it is gone.
        async fn task(&self, task_id: &str) -> Result<Option<TaskFacts>>;

        async fn columns_for_board(&self, board_id: &str) -> Result<Vec<Column>>;

        async fn assignees_for_task(&self, task_id: &str) -> Result<Vec<Person>>;

        /// Everyone in the workspace who may be given work.
        async fn assignable_people(&self, workspace_id: &str) -> Result<Vec<Person>>;

        /// Both directions at once: `(is_blocked_by, edge)`. `true` means the
        /// other task blocks this one.
        async fn dependencies_for_task(&self, task_id: &str)
        -> Result<Vec<(bool, DependencyEdge)>>;

        async fn comments_for_task(&self, task_id: &str) -> Result<Vec<Comment>>;

        /// The files hung off a task, without their bytes.
        async fn files_for_task(&self, task_id: &str) -> Result<Vec<FileLine>>;

        async fn activity_for_task(&self, task_id: &str) -> Result<Vec<ActivityEntry>>;
    }

    /// Loads one task detail in eight queries — the task, the board's columns,
    /// its assignees, the workspace's writers, both directions of its
    /// dependencies, its comments, its files and its activity.
    ///
    /// Eight, not eight-plus-one-per-comment: `a_task_detail_costs_eight_
    /// queries_whatever_it_carries` in `tests/store.rs` holds that line.
    pub async fn load(
        reads: &dyn DetailReads,
        workspace_id: &str,
        task_id: &str,
    ) -> Result<Option<TaskDetail>> {
        let Some(facts) = reads.task(task_id).await? else {
            return Ok(None);
        };
        // A task id from another workspace is not found, not forbidden: the
        // asker is told nothing about a board they cannot see.
        if facts.workspace_id != workspace_id {
            return Ok(None);
        }
        let task = facts.row;
        let columns = reads.columns_for_board(&facts.board_id).await?;
        let assignees = reads.assignees_for_task(task_id).await?;
        let assignable = reads.assignable_people(workspace_id).await?;
        let edges = reads.dependencies_for_task(task_id).await?;
        let comments = reads.comments_for_task(task_id).await?;
        let files = reads.files_for_task(task_id).await?;
        let activity = reads.activity_for_task(task_id).await?;

        let Some(column) = columns
            .iter()
            .find(|column| column.id == task.column_id)
            .cloned()
        else {
            return Ok(None);
        };
        let mut blocked_by = Vec::new();
        let mut blocks = Vec::new();
        for (is_blocked_by, edge) in edges {
            if is_blocked_by {
                blocked_by.push(edge);
            } else {
                blocks.push(edge);
            }
        }

        Ok(Some(TaskDetail {
            id: task.id,
            task_key: task.task_key,
            title: task.title,
            description: facts.description,
            column,
            columns,
            deadline: task.deadline,
            done_at: task.done_at,
            assignees,
            assignable,
            blocked_by,
            blocks,
            comments,
            files,
            activity,
        }))
    }
}
