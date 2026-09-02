//! The board vocabulary: columns, cards and the numbers the Main artboard puts
//! in its chrome.
//!
//! This module is deliberately free of the `server` feature: the browser bundle
//! renders these exact types, so a card means the same thing on both sides and
//! nothing has to be re-derived from a wire format.

use serde::{Deserialize, Serialize};
use time::{Date, Month, OffsetDateTime};

/// A board and the key prefix its tasks carry (`DZ-14`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardMeta {
    pub id: String,
    pub name: String,
    pub task_prefix: String,
}

/// A column. `is_done` is what turns a card into the finished, greyed state
/// with a "done Aug 14" stamp rather than a deadline chip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Column {
    pub id: String,
    pub name: String,
    pub position: i64,
    pub is_done: bool,
}

/// One task as the store holds it, before assignees, comments and dependencies
/// are hung off it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskRow {
    pub id: String,
    pub task_key: String,
    pub title: String,
    pub column_id: String,
    pub deadline: Option<Date>,
    /// The meeting instant — an exact time, unlike the day-grain deadline.
    pub clock_at: Option<OffsetDateTime>,
    pub position: f64,
    pub done_at: Option<OffsetDateTime>,
    /// The task this one is a subtask of. A row with a parent gets no card of
    /// its own on the board — it is counted on its parent's instead — but it
    /// is still read with the rest, because a key it owns can appear on
    /// somebody else's card as a blocker.
    pub parent_id: Option<String>,
    /// The tag — the project — the task wears, if any. The name rides along
    /// so a card can show a chip without a second sweep.
    pub tag: Option<TagChip>,
}

/// One card crossing from one column into another, as it was written.
///
/// `at` is the moment the move committed, taken inside the same transaction
/// as the move. The mail engine reads it rather than stamping its own clock:
/// a send retried on Thursday still has to say the card moved on Tuesday.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transition {
    pub id: String,
    pub task_id: String,
    pub from_column: String,
    pub to_column: String,
    pub actor_id: String,
    pub at: OffsetDateTime,
}

/// What a move attempt did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Moved {
    /// The card changed column. This is the fact that was written, and the
    /// only outcome the mail rules act on.
    Recorded(Transition),
    /// The card was dropped back in the column it came from. Nothing was
    /// written: that is not a transition, and a rule must not fire for it.
    Unchanged,
    /// Somebody else moved the card between the drag starting and the drop
    /// landing, so the column this move was told to move it out of is no
    /// longer the column it is in. Nothing was written and the caller should
    /// re-read the board rather than overwrite a decision it never saw.
    Stale,
    /// The card was headed for a done column with subtasks still open. A
    /// parent finished while its parts are not is a lie the board would then
    /// go on telling, so nothing was written.
    ///
    /// There is no override: a subtask nobody will do is deleted or promoted
    /// out of its parent, both of which are single writes that already exist.
    /// A "finish anyway" button would make this rule advisory, which is worse
    /// than not having it.
    Held,
}

/// Whoever a card can point at. The address is not here: a board card never
/// shows one, so it never leaves the server for this screen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Person {
    pub id: String,
    pub display_name: String,
    pub has_photo: bool,
}

impl Person {
    /// The two-letter fallback the artboard uses when there is no photo (`MD`).
    pub fn initials(&self) -> String {
        let mut letters = self
            .display_name
            .split_whitespace()
            .filter_map(|word| word.chars().next())
            .filter(|c| c.is_alphanumeric());
        let first = letters.next();
        let second = letters.next();
        match (first, second) {
            (Some(a), Some(b)) => format!("{a}{b}").to_uppercase(),
            (Some(a), None) => a.to_uppercase().to_string(),
            _ => "?".to_string(),
        }
    }
}

/// A tag's id and name, as a card and the detail carry them: enough to show
/// a chip and to filter the board on, and nothing more.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagChip {
    pub id: String,
    pub name: String,
}

/// A card, assembled. Dependencies carry the *keys* of the tasks on the other
/// end, because that is what the card shows and it saves the UI a second lookup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskCard {
    pub id: String,
    pub task_key: String,
    pub title: String,
    pub column_id: String,
    pub deadline: Option<Date>,
    /// The meeting instant the card owes its people a reminder about, when
    /// it has one.
    pub clock_at: Option<OffsetDateTime>,
    pub done_at: Option<OffsetDateTime>,
    pub position: f64,
    pub assignees: Vec<Person>,
    pub comment_count: u32,
    /// Keys of the tasks that must finish before this one can start.
    pub blocked_by: Vec<String>,
    /// Keys of the tasks waiting on this one.
    pub blocks: Vec<String>,
    /// How many subtasks this card has, and how many of them are finished.
    /// Both are zero for a card with none, which is what hides the chip.
    pub subtask_total: u32,
    pub subtask_done: u32,
    /// The tag the card wears — its project — when it has one.
    pub tag: Option<TagChip>,
}

impl TaskCard {
    pub fn is_done(&self) -> bool {
        self.done_at.is_some()
    }

    /// The `2/5` the card wears, or nothing when the task has no parts. It is
    /// also the only place the count appears when a move is refused, which is
    /// why the refusal itself does not carry one.
    pub fn subtask_label(&self) -> Option<String> {
        (self.subtask_total > 0).then(|| format!("{}/{}", self.subtask_done, self.subtask_total))
    }

    /// Whether finishing this card is currently refused.
    pub fn holds_on_subtasks(&self) -> bool {
        self.subtask_done < self.subtask_total
    }

    /// Blocked means something unfinished is in front of it. A finished task is
    /// never counted as blocked, whatever it still points at.
    pub fn is_blocked(&self) -> bool {
        !self.is_done() && !self.blocked_by.is_empty()
    }

    /// Past its deadline and not finished. A deadline of today is not overdue.
    pub fn is_overdue(&self, today: Date) -> bool {
        match self.deadline {
            Some(day) => !self.is_done() && day < today,
            None => false,
        }
    }

    pub fn is_assigned_to(&self, user_id: &str) -> bool {
        self.assignees.iter().any(|person| person.id == user_id)
    }

    /// The chip the card shows where a deadline would go: `Sep 12`,
    /// `Aug 21 · overdue`, `done Aug 14` or `no deadline`. English-only —
    /// kept for callers (mail, tests) that want the baked sentence; UI
    /// rendering should use [`TaskCard::deadline_parts`] and translate.
    pub fn deadline_label(&self, today: Date) -> String {
        if let Some(done) = self.done_at {
            return format!("done {}", day_label(done.date()));
        }
        match self.deadline {
            Some(day) if day < today => format!("{} · overdue", day_label(day)),
            Some(day) => day_label(day),
            None => "no deadline".to_string(),
        }
    }

    /// The same chip as [`TaskCard::deadline_label`], split into a
    /// language-free date string and a state the caller translates.
    /// `None` when there's nothing to show (no deadline, not done).
    pub fn deadline_parts(&self, today: Date) -> Option<DeadlineParts> {
        if let Some(done) = self.done_at {
            return Some(DeadlineParts {
                date: day_label(done.date()),
                state: DeadlineState::Done,
            });
        }
        self.deadline.map(|day| DeadlineParts {
            date: day_label(day),
            state: if day < today {
                DeadlineState::Overdue
            } else {
                DeadlineState::OnTime
            },
        })
    }
}

/// A deadline chip's date text plus what state it's in — the pieces a UI
/// layer combines with its own translated words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeadlineParts {
    pub date: String,
    pub state: DeadlineState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeadlineState {
    OnTime,
    Overdue,
    Done,
}

/// A column with its cards already in it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnView {
    pub column: Column,
    pub cards: Vec<TaskCard>,
}

/// Everything one board screen needs, in one value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoardView {
    pub board: BoardMeta,
    pub columns: Vec<ColumnView>,
}

impl BoardView {
    pub fn cards(&self) -> impl Iterator<Item = &TaskCard> {
        self.columns.iter().flat_map(|column| column.cards.iter())
    }

    pub fn task_count(&self) -> usize {
        self.cards().count()
    }

    pub fn is_empty(&self) -> bool {
        self.task_count() == 0
    }

    /// The `1 overdue` chip in the filter bar.
    pub fn overdue_count(&self, today: Date) -> usize {
        self.cards().filter(|card| card.is_overdue(today)).count()
    }

    /// The `2 blocked` chip beside it.
    pub fn blocked_count(&self) -> usize {
        self.cards().filter(|card| card.is_blocked()).count()
    }

    /// Keeps only the cards whose key or title answers `query`: a literal
    /// substring of the folded key or folded title. The human key typed in
    /// any case matches; a fragment of a title matches; nothing is
    /// pattern-matched, so there are no wildcards to escape.
    pub fn searching(&mut self, query: &str) {
        let needle = fold(query);
        for column in &mut self.columns {
            column.cards.retain(|card| {
                fold(&card.task_key).contains(&needle) || fold(&card.title).contains(&needle)
            });
        }
    }

    /// Keeps only the cards wearing `tag`, when one is named — the Project
    /// filter, riding the same narrowing pass as [`BoardView::searching`].
    pub fn tagged(&mut self, tag_id: Option<&str>) {
        let Some(tag_id) = tag_id else {
            return;
        };
        for column in &mut self.columns {
            column
                .cards
                .retain(|card| card.tag.as_ref().is_some_and(|tag| tag.id == tag_id));
        }
    }
}

/// Folds text for search: Turkish letters collapse to their ASCII stems
/// (`İ`/`I`/`ı` → `i`, `ş` → `s`, `ç` → `c`, `ğ` → `g`, `ü` → `u`, `ö` →
/// `o`), case flattens. A naive lowercase cannot stand in for this: it
/// turns `İ` into `i` plus a combining dot, and then no plain-`i` query —
/// `is`, say, against a card titled `İş Planı Teslimi` — can ever match.
/// Query and card fold through the same map, so `iş`, `İŞ`, `is`, and
/// `İS` all find `İş`.
fn fold(text: &str) -> String {
    let mut folded = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            'İ' | 'I' | 'ı' => folded.push('i'),
            'ş' | 'Ş' => folded.push('s'),
            'ç' | 'Ç' => folded.push('c'),
            'ğ' | 'Ğ' => folded.push('g'),
            'ü' | 'Ü' => folded.push('u'),
            'ö' | 'Ö' => folded.push('o'),
            other => folded.extend(other.to_lowercase()),
        }
    }
    folded
}

/// `Sep 12` — the card chips are day-and-month, never a year.
pub fn day_label(day: Date) -> String {
    let month = match day.month() {
        Month::January => "Jan",
        Month::February => "Feb",
        Month::March => "Mar",
        Month::April => "Apr",
        Month::May => "May",
        Month::June => "Jun",
        Month::July => "Jul",
        Month::August => "Aug",
        Month::September => "Sep",
        Month::October => "Oct",
        Month::November => "Nov",
        Month::December => "Dec",
    };
    format!("{month} {:02}", day.day())
}

/// Builds the board from six flat sweeps.
///
/// Every input is the whole board's worth of one thing, so the caller makes a
/// fixed number of queries no matter how many tasks there are — see
/// [`load`](crate::board::load). The joining happens here, in Rust.
pub fn assemble(
    board: BoardMeta,
    mut columns: Vec<Column>,
    tasks: Vec<TaskRow>,
    assignees: Vec<(String, Person)>,
    comment_counts: Vec<(String, u32)>,
    dependencies: Vec<(String, String)>,
) -> BoardView {
    use std::collections::HashMap;

    let key_of: HashMap<&str, &str> = tasks
        .iter()
        .map(|task| (task.id.as_str(), task.task_key.as_str()))
        .collect();

    let mut people: HashMap<String, Vec<Person>> = HashMap::new();
    for (task_id, person) in assignees {
        people.entry(task_id).or_default().push(person);
    }
    let counts: HashMap<String, u32> = comment_counts.into_iter().collect();

    let finished: HashMap<&str, bool> = tasks
        .iter()
        .map(|task| (task.id.as_str(), task.done_at.is_some()))
        .collect();

    let mut blocked_by: HashMap<String, Vec<String>> = HashMap::new();
    let mut blocks: HashMap<String, Vec<String>> = HashMap::new();
    for (blocked, blocking) in &dependencies {
        // A dependency on a task from another board would have no key here; it
        // is dropped rather than shown as a dangling chip. A dependency whose
        // other end is finished is dropped too: nothing is waiting on a task
        // that is done, whatever the row still says.
        let done = |id: &str| finished.get(id).copied().unwrap_or(false);
        if let Some(key) = key_of.get(blocking.as_str())
            && !done(blocking)
        {
            blocked_by
                .entry(blocked.clone())
                .or_default()
                .push((*key).to_string());
        }
        if let Some(key) = key_of.get(blocked.as_str())
            && !done(blocked)
        {
            blocks
                .entry(blocking.clone())
                .or_default()
                .push((*key).to_string());
        }
    }

    // A subtask is counted on its parent's card, never given one of its own.
    let mut subtask_total: HashMap<&str, u32> = HashMap::new();
    let mut subtask_done: HashMap<&str, u32> = HashMap::new();
    for task in &tasks {
        if let Some(parent) = task.parent_id.as_deref() {
            *subtask_total.entry(parent).or_default() += 1;
            if task.done_at.is_some() {
                *subtask_done.entry(parent).or_default() += 1;
            }
        }
    }
    let subtask_total: HashMap<String, u32> = subtask_total
        .into_iter()
        .map(|(id, n)| (id.to_string(), n))
        .collect();
    let subtask_done: HashMap<String, u32> = subtask_done
        .into_iter()
        .map(|(id, n)| (id.to_string(), n))
        .collect();

    let mut cards: HashMap<String, Vec<TaskCard>> = HashMap::new();
    for task in tasks {
        // Read with the rest so its key can appear as a blocker chip, but the
        // board is a board of tasks: a subtask reaches its page through its
        // parent's.
        if task.parent_id.is_some() {
            continue;
        }
        let card = TaskCard {
            comment_count: counts.get(&task.id).copied().unwrap_or(0),
            assignees: people.remove(&task.id).unwrap_or_default(),
            blocked_by: blocked_by.remove(&task.id).unwrap_or_default(),
            blocks: blocks.remove(&task.id).unwrap_or_default(),
            subtask_total: subtask_total.get(&task.id).copied().unwrap_or(0),
            subtask_done: subtask_done.get(&task.id).copied().unwrap_or(0),
            id: task.id,
            task_key: task.task_key,
            title: task.title,
            deadline: task.deadline,
            clock_at: task.clock_at,
            done_at: task.done_at,
            position: task.position,
            column_id: task.column_id,
            tag: task.tag,
        };
        cards.entry(card.column_id.clone()).or_default().push(card);
    }

    columns.sort_by_key(|column| column.position);
    let columns = columns
        .into_iter()
        .map(|column| {
            let mut cards = cards.remove(&column.id).unwrap_or_default();
            sort_cards(&mut cards);
            ColumnView { column, cards }
        })
        .collect();

    BoardView { board, columns }
}

/// The board's default order, which the artboard names "Sort: deadline":
/// soonest deadline first, cards without one after them, ties broken by the
/// hand-set position so a column never reshuffles on its own.
fn sort_cards(cards: &mut [TaskCard]) {
    cards.sort_by(|a, b| {
        (a.deadline.is_none(), a.deadline)
            .cmp(&(b.deadline.is_none(), b.deadline))
            .then(a.position.total_cmp(&b.position))
            .then_with(|| a.task_key.cmp(&b.task_key))
    });
}

#[cfg(feature = "server")]
pub use reads::{BoardReads, load};

#[cfg(feature = "server")]
mod reads {
    use async_trait::async_trait;

    use super::{BoardMeta, BoardView, Column, Person, TaskRow, assemble};
    use crate::store::Result;

    /// The board's read half, split out from [`Store`](crate::Store) so a test
    /// can wrap it and count the round trips a board costs.
    ///
    /// Every method sweeps the whole board. There is deliberately no
    /// "assignees for this task" here: one query per card is how a board with
    /// fourteen tasks becomes sixty round trips against a single-writer file.
    #[async_trait]
    pub trait BoardReads: Send + Sync {
        /// The workspace's board. There is one today; the type is ready for
        /// more.
        async fn board(&self, workspace_id: &str) -> Result<Option<BoardMeta>>;

        async fn columns(&self, board_id: &str) -> Result<Vec<Column>>;

        /// Live tasks, deleted ones left out.
        async fn tasks_for_board(&self, board_id: &str) -> Result<Vec<TaskRow>>;

        /// `(task id, person)` for every assignment on the board.
        async fn assignees_for_board(&self, board_id: &str) -> Result<Vec<(String, Person)>>;

        /// `(task id, comments)`, tasks with none left out.
        async fn comment_counts_for_board(&self, board_id: &str) -> Result<Vec<(String, u32)>>;

        /// `(blocked task id, blocking task id)` for every dependency still in
        /// force.
        async fn dependencies_for_board(&self, board_id: &str) -> Result<Vec<(String, String)>>;
    }

    /// Loads a whole board in six queries — one for the board and one sweep per
    /// thing a card carries — and joins them in Rust.
    ///
    /// The count does not move with the number of tasks; `board_costs_the_same_
    /// six_queries_at_any_size` in `tests/store.rs` holds that line.
    pub async fn load(reads: &dyn BoardReads, workspace_id: &str) -> Result<Option<BoardView>> {
        let Some(board) = reads.board(workspace_id).await? else {
            return Ok(None);
        };
        let columns = reads.columns(&board.id).await?;
        let tasks = reads.tasks_for_board(&board.id).await?;
        let assignees = reads.assignees_for_board(&board.id).await?;
        let comment_counts = reads.comment_counts_for_board(&board.id).await?;
        let dependencies = reads.dependencies_for_board(&board.id).await?;
        Ok(Some(assemble(
            board,
            columns,
            tasks,
            assignees,
            comment_counts,
            dependencies,
        )))
    }
}
