-- Izlek's schema, whole, in one file.
--
-- One plain SQL file, applied at boot to an empty database: Turso has no
-- migration runner of its own, and keeping the schema as SQL is what makes
-- the store trait swappable.
-- Timestamps are RFC 3339 text in UTC. Ids are ULIDs as text — sortable by
-- creation, and Crockford-uppercase already, which is what lets a task key
-- borrow its tail.
--
-- This file is the whole schema, and it is edited in place: a change to a
-- table is made here, in the table, never appended as an ALTER. An existing
-- database is brought up to it by `izlek reconcile`, not by the store, so a
-- change to a table lands in two places — here, and in that tool's copy map.

CREATE TABLE workspace (
    id                     TEXT PRIMARY KEY,
    name                   TEXT NOT NULL,
    created_at             TEXT NOT NULL,
    -- Limits. Each states its consequence in the UI, not here. They are
    -- workspace content — an admin edits them, and no restart should be
    -- needed to raise an attachment ceiling.
    attachment_limit_bytes INTEGER NOT NULL DEFAULT 26214400,
    photo_limit_bytes      INTEGER NOT NULL DEFAULT 2097152,
    -- JSON array of allowed extensions. An empty array means every type.
    allowed_file_types     TEXT NOT NULL DEFAULT '[]',
    -- How long a notification waits for the rest of its workflow. One normal
    -- sequence — open a card, fix the column, assign it, set a deadline — is
    -- four writes and was four mails; held for a few quiet minutes it is one
    -- mail saying where the card ended up. Zero sends each one the moment it
    -- is owed, which is what this did before the column existed.
    mail_batch_minutes     INTEGER NOT NULL DEFAULT 5,
    -- How long before a task's clock its reminder mail is queued. Zero turns
    -- reminders off.
    reminder_minutes       INTEGER NOT NULL DEFAULT 15,
    -- The single sender every mail in the workspace goes through, written by
    -- the admin in Settings rather than by whoever has a shell on the box: a
    -- three-person team has those be different people, and the second one is
    -- not always awake.
    --
    -- `smtp_password` holds ciphertext, never the typed string. The server
    -- has to present the password to the mail host on every send, so a
    -- one-way hash is impossible; instead `store::secret` seals it under a
    -- key kept in `izlek.key` beside this file, so a database copied for a
    -- backup does not carry a working credential with it. No field on
    -- `Workspace` exposes it — it is written by `Store::set_sender` and read
    -- by `Store::smtp_password`, which only the mailer calls.
    smtp_host              TEXT,
    smtp_port              INTEGER,
    smtp_username          TEXT,
    smtp_password          TEXT,
    smtp_from_name         TEXT,
    smtp_from_address      TEXT,
    -- The result of the last "send a test mail to myself", kept rather than
    -- shown once: an admin fills the sender in, presses the button, and comes
    -- back tomorrow wondering whether it ever worked. `smtp_test_error` is the
    -- mail server's own words, so the admin can act on them; the mailer builds
    -- its errors from what the server said, never from the credentials it sent.
    smtp_test_at           TEXT,
    smtp_test_ms           INTEGER,
    smtp_test_error        TEXT,
    -- Whether the mail server let us in, which is a different fact from
    -- whether a mail was delivered. `smtp_test_*` above records a real
    -- message going out to a real inbox; these record a handshake —
    -- connect, negotiate TLS, say hello, authenticate, hang up. It proves
    -- the host, the port, the encryption and the credentials, and proves
    -- nothing about whether the from-address is one this account may send
    -- as — a server that accepts the login can still refuse the envelope.
    -- Kept apart so neither can be read as the other. Cleared whenever the
    -- sender is edited, for the same reason the test result is: it was
    -- about settings that no longer exist.
    smtp_check_at           TEXT,
    smtp_check_ms           INTEGER,
    smtp_check_error        TEXT,
    -- The address mail links point at, when the one the process was
    -- configured with is not the one people reach. A box behind a proxy
    -- answers on localhost and is known by a public name, and only an admin
    -- knows which — so it is workspace content, and `config/izlek.toml`'s
    -- `base_url` is what an empty one falls back to.
    public_url             TEXT
);

CREATE TABLE user (
    id                TEXT PRIMARY KEY,
    workspace_id      TEXT NOT NULL REFERENCES workspace(id),
    email             TEXT NOT NULL,
    display_name      TEXT NOT NULL,
    -- 'admin' | 'member' | 'viewer'
    role              TEXT NOT NULL,
    -- NULL until the person picks a password on first sign-in. The admin
    -- creates the account with a name and an address and nothing else.
    password_hash     TEXT,
    -- Whoever made this account, so the first-sign-in screen can greet the
    -- invited person by the inviter's name instead of reading their own name
    -- back to them. NULL for the first account, which nobody invited. A
    -- reference rather than a copied name, so a later rename is reflected.
    invited_by        TEXT REFERENCES user(id),
    -- Display preferences. Stored data everywhere else stays UTC and
    -- language-neutral; these only change how a browser renders it for the
    -- one person signed in as this user, and nothing stored depends on them.
    timezone          TEXT NOT NULL DEFAULT 'UTC',
    theme             TEXT NOT NULL DEFAULT 'light',
    language          TEXT NOT NULL DEFAULT 'en',
    ui                TEXT NOT NULL DEFAULT 'instrument',
    -- The profile picture lives here as bytes, the way an attachment does:
    -- no filesystem path survives a backup or a move.
    photo             BLOB,
    photo_mime        TEXT,
    created_at        TEXT NOT NULL,
    last_signed_in_at TEXT,
    -- Where the person's "what changed for me" feed has been read to. NULL
    -- until their first visit, which reads every line as news.
    feed_seen_at      TEXT
);
CREATE UNIQUE INDEX user_email_unique ON user(workspace_id, email);

-- One workspace, one owner. The singleton column is the whole point: the row
-- can exist at most once, so a second claim cannot race a first one.
CREATE TABLE workspace_owner (
    singleton  INTEGER PRIMARY KEY,
    user_id    TEXT NOT NULL REFERENCES user(id),
    claimed_at TEXT NOT NULL
);

-- First sign-in / resend links. Only the hash is stored: the plaintext is
-- shown once when the admin creates or resends it.
CREATE TABLE signin_link (
    id         TEXT PRIMARY KEY,
    user_id    TEXT NOT NULL REFERENCES user(id),
    token_hash TEXT NOT NULL,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    used_at    TEXT,
    kind       TEXT NOT NULL DEFAULT 'join'
);
CREATE UNIQUE INDEX signin_link_token ON signin_link(token_hash);

CREATE TABLE session (
    id         TEXT PRIMARY KEY,
    user_id    TEXT NOT NULL REFERENCES user(id),
    token_hash TEXT NOT NULL,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    revoked_at TEXT
);
CREATE UNIQUE INDEX session_token ON session(token_hash);
CREATE INDEX session_by_user ON session(user_id);

-- The rate-limit ledger: one row per attempt, pruned by age.
CREATE TABLE auth_attempt (
    id           TEXT PRIMARY KEY,
    bucket       TEXT NOT NULL,
    attempted_at TEXT NOT NULL
);
CREATE INDEX auth_attempt_by_bucket ON auth_attempt(bucket, attempted_at);

CREATE TABLE board (
    id           TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL REFERENCES workspace(id),
    name         TEXT NOT NULL,
    -- The key prefix this board's tasks carry (`DZ-14`).
    task_prefix  TEXT NOT NULL DEFAULT 'DZ',
    created_at   TEXT NOT NULL
);

CREATE TABLE board_column (
    id       TEXT PRIMARY KEY,
    board_id TEXT NOT NULL REFERENCES board(id),
    name     TEXT NOT NULL,
    position INTEGER NOT NULL,
    -- The column a finished task lands in; drives the "done Aug 14" card state.
    is_done  INTEGER NOT NULL DEFAULT 0
);

-- A tag is the project a task belongs to. A task wears at most one, so the
-- link is a column on `task` rather than a join table: two projects on one
-- task is a mistake the schema can refuse outright.
--
-- A tag belongs to a board, as a mail rule does. Its order is the admin's —
-- set by hand, up and down — so it is stored rather than derived from
-- anything the board already knows.
--
-- `is_default` marks the board's one fallback: every task must wear a tag,
-- so the board has one tag that catches whatever loses its own. That there
-- is exactly one is the partial index's job, not the store's.
CREATE TABLE tag (
    id         TEXT PRIMARY KEY,
    board_id   TEXT NOT NULL REFERENCES board(id),
    name       TEXT NOT NULL,
    position   INTEGER NOT NULL,
    is_default INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL
);
CREATE UNIQUE INDEX tag_one_default ON tag(board_id) WHERE is_default = 1;
CREATE UNIQUE INDEX tag_name_unique ON tag(board_id, name);
CREATE INDEX tag_by_board ON tag(board_id, position);

-- A task, and a subtask, which is the same thing with a parent.
--
-- `parent_id` is the whole of the subtask feature in this schema: a subtask
-- has every column a task has, so it carries assignees, a deadline, comments,
-- files, activity and mail exactly like anything else, and promoting one to
-- a task of its own is a single UPDATE rather than a move between tables.
--
-- Two rules the column cannot state itself, enforced in the store instead:
-- a parent must have `parent_id IS NULL` (one level, never a tree), and a
-- child sits on its parent's board. SQLite cannot ask, in a CHECK, whether
-- the row a foreign key points at has a NULL parent.
--
-- The board shows only rows with no parent; a subtask reaches its own detail
-- page through its parent's.
CREATE TABLE task (
    id          TEXT PRIMARY KEY,
    board_id    TEXT NOT NULL REFERENCES board(id),
    parent_id   TEXT REFERENCES task(id),
    -- Human key from the mockups, e.g. DZ-14. The tail is the end of the
    -- task's own ULID rather than a per-board counter: a counter leaves
    -- visible gaps once tasks are deleted.
    task_key    TEXT NOT NULL,
    title       TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    column_id   TEXT NOT NULL REFERENCES board_column(id),
    -- The project the task belongs to — NOT NULL on purpose, which a
    -- declared schema can say where an ALTER could not: every task wears a
    -- tag, and the board's default one catches whatever loses its own.
    tag_id      TEXT NOT NULL REFERENCES tag(id),
    deadline    TEXT,
    -- The meeting instant: an exact date and time, unlike the day-granularity
    -- deadline beside it. Full RFC 3339 UTC stamp, like the other stamps, so
    -- it sorts and compares as text; NULL is a task with no clock.
    clock_at    TEXT,
    position    REAL NOT NULL DEFAULT 0,
    created_by  TEXT NOT NULL REFERENCES user(id),
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    done_at     TEXT,
    deleted_at  TEXT
);
CREATE UNIQUE INDEX task_key_unique ON task(board_id, task_key);
CREATE INDEX task_by_column ON task(column_id);
CREATE INDEX task_by_parent ON task(parent_id);

CREATE TABLE task_assignee (
    task_id TEXT NOT NULL REFERENCES task(id),
    user_id TEXT NOT NULL REFERENCES user(id),
    PRIMARY KEY (task_id, user_id)
);

CREATE TABLE task_watcher (
    task_id TEXT NOT NULL REFERENCES task(id),
    user_id TEXT NOT NULL REFERENCES user(id),
    PRIMARY KEY (task_id, user_id)
);

-- blocked_task_id is blocked by blocking_task_id. cleared_at is set when the
-- blocking task finishes, which is what the "you can start now" rule reads.
--
-- The CHECK is new: a task blocking itself was only ever stopped in
-- application code, and a row like that would make a card permanently and
-- unfixably blocked by its own existence.
CREATE TABLE task_dependency (
    blocked_task_id  TEXT NOT NULL REFERENCES task(id),
    blocking_task_id TEXT NOT NULL REFERENCES task(id),
    created_at       TEXT NOT NULL,
    cleared_at       TEXT,
    PRIMARY KEY (blocked_task_id, blocking_task_id),
    CHECK (blocked_task_id <> blocking_task_id)
);
-- The primary key answers "what is this task waiting on"; the other
-- direction — "who is waiting on this one", which every finish and every
-- delete asks — has no prefix of it to use.
CREATE INDEX task_dependency_by_blocking ON task_dependency(blocking_task_id);

CREATE TABLE comment (
    id         TEXT PRIMARY KEY,
    task_id    TEXT NOT NULL REFERENCES task(id),
    author_id  TEXT NOT NULL REFERENCES user(id),
    body       TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX comment_by_task ON comment(task_id);

-- Attachments live in the database file, not on disk beside it.
--
-- Izlek is one process over one file, and a second place to keep state is a
-- second thing to back up, to lock, and to get out of step with the rows that
-- name it. And a path is exactly where an uploaded file name must never end
-- up: `../../etc` is a valid file name and a terrible path. Keeping the bytes
-- here means `file_name` is only ever a label printed on a chip, never
-- anything the server resolves.
CREATE TABLE attachment (
    id          TEXT PRIMARY KEY,
    task_id     TEXT NOT NULL REFERENCES task(id),
    comment_id  TEXT REFERENCES comment(id),
    file_name   TEXT NOT NULL,
    mime_type   TEXT NOT NULL,
    size_bytes  INTEGER NOT NULL,
    bytes       BLOB NOT NULL,
    uploaded_by TEXT NOT NULL REFERENCES user(id),
    created_at  TEXT NOT NULL
);
CREATE INDEX attachment_by_task ON attachment(task_id);

-- Every move of a card from one column to another, written as a fact in the
-- same transaction as the move itself.
--
-- The mail rules trigger on the transition, not on the task's current column:
-- a card that goes Review -> Done -> Review has crossed into Done once, and
-- nothing recomputed from `task.column_id` afterwards can tell you that.
--
-- `from_column` deliberately carries no REFERENCES: a task created straight
-- into a column writes a transition with `from_column = ''`, meaning "created
-- into", so a rule watching that column fires for a new card and not only for
-- one dragged in later. No column's id is ever ''.
CREATE TABLE transition (
    id          TEXT PRIMARY KEY,
    task_id     TEXT NOT NULL REFERENCES task(id),
    from_column TEXT NOT NULL,
    to_column   TEXT NOT NULL REFERENCES board_column(id),
    actor_id    TEXT NOT NULL REFERENCES user(id),
    created_at  TEXT NOT NULL
);
CREATE INDEX transition_by_task ON transition(task_id, created_at);

-- A deletion that let other tasks go, recorded as a fact.
--
-- The unblocked rule has two causes and only one of them is a crossing: a
-- blocker can finish, or a blocker can be deleted. The crossing already has a
-- row in `transition`; the deletion had nowhere to be written, so a mail owed
-- because of one had no event to hang from and no way to be rebuilt on a
-- retry. This is that row.
--
-- `cause_key` and `cause_title` are copied rather than joined: the task they
-- name is deleted, and a mail sent an hour later still has to be able to say
-- which task it was.
CREATE TABLE freeing (
    id          TEXT PRIMARY KEY,
    board_id    TEXT NOT NULL REFERENCES board(id),
    cause_key   TEXT NOT NULL,
    cause_title TEXT NOT NULL,
    actor_id    TEXT NOT NULL REFERENCES user(id),
    created_at  TEXT NOT NULL
);
CREATE INDEX freeing_by_board ON freeing(board_id, created_at);

-- The activity strip in the task detail: who did what. `task_id` is nullable
-- because account and admin events — sign-ins, invites, role changes — have
-- no task to sit under, and still have to be loggable.
CREATE TABLE activity (
    id         TEXT PRIMARY KEY,
    task_id    TEXT REFERENCES task(id),
    actor_id   TEXT REFERENCES user(id),
    -- Who an activity line is about, as opposed to the actor who did it:
    -- an Assigned line is about the person just assigned, an Unassigned one
    -- about the person just removed. A mail rule aimed at the assignees
    -- re-derives its one recipient from this id, on a retry as well as on
    -- the first run — the detail column carries only a display name, and
    -- two people can share a name. NULL for lines that are about nobody: a
    -- comment, a move, a delete.
    subject_id TEXT REFERENCES user(id),
    kind       TEXT NOT NULL,
    detail     TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL
);
CREATE INDEX activity_by_task ON activity(task_id);
CREATE INDEX activity_recent ON activity(created_at);

-- Mail rules are written only by the admin.
--
-- `trigger_kind` carries no CHECK on purpose: the set of triggers grows
-- without a schema change. `audience` does carry one, because those three
-- values are the only audiences the engine knows how to resolve.
CREATE TABLE mail_rule (
    id                   TEXT PRIMARY KEY,
    board_id             TEXT NOT NULL REFERENCES board(id),
    trigger_kind         TEXT NOT NULL,
    trigger_column       TEXT REFERENCES board_column(id),
    subject              TEXT NOT NULL,
    -- 'assignees' — the people the card points at; 'board' — everyone who can
    -- write on the board; 'creator' — whoever opened the card.
    audience             TEXT NOT NULL CHECK (audience IN ('assignees', 'board', 'creator')),
    enabled              INTEGER NOT NULL DEFAULT 1,
    -- Fold the task's own facts into the body instead of the subject line
    -- being all the recipient reads. Off by default.
    include_task_details INTEGER NOT NULL DEFAULT 0,
    created_at           TEXT NOT NULL,
    -- Only a status rule names a column, and a status rule that names none
    -- watches every column. The check is here so a rule that names a column
    -- it could not act on cannot be stored at all.
    CHECK (trigger_column IS NULL OR trigger_kind = 'status')
);

-- Four shapes share the table. A 'rule' send is owed to somebody because a
-- rule matched an event on a task, and carries all three. An 'invite', a
-- 'notice' or a 'reminder' has none of them — a person is invited before they
-- have a task, a transition, or a rule to owe them anything, and a reminder
-- is minted straight onto the clock it serves — and carries its own subject
-- and body instead of getting them from `mail_rule`. The two CHECKs make the
-- shapes exclusive, so a half-built row cannot be stored.
--
-- `event_id` names a transition or a freeing without a foreign key: two
-- tables can cause a send, so the column cannot point at either and hold both.
CREATE TABLE mail_send (
    id              TEXT PRIMARY KEY,
    rule_id         TEXT REFERENCES mail_rule(id) ON DELETE CASCADE,
    event_id        TEXT,
    task_id         TEXT REFERENCES task(id),
    recipient       TEXT NOT NULL,
    state           TEXT NOT NULL CHECK (state IN ('pending', 'sent', 'failed', 'abandoned')),
    attempts        INTEGER NOT NULL DEFAULT 0,
    last_error      TEXT,
    claimed_at      TEXT NOT NULL,
    next_attempt_at TEXT,
    sent_at         TEXT,
    kind            TEXT NOT NULL DEFAULT 'rule' CHECK (kind IN ('rule', 'invite', 'notice', 'reminder')),
    subject         TEXT,
    body            TEXT,
    CHECK ((kind = 'rule') = (rule_id IS NOT NULL AND event_id IS NOT NULL AND task_id IS NOT NULL)),
    CHECK ((kind IN ('invite', 'notice', 'reminder')) = (subject IS NOT NULL AND body IS NOT NULL))
);
CREATE UNIQUE INDEX mail_send_once ON mail_send(rule_id, event_id, task_id, recipient);
CREATE INDEX mail_send_owed ON mail_send(next_attempt_at);
CREATE INDEX mail_send_by_rule ON mail_send(rule_id, sent_at);
-- The task's own mail strip reads by task, which neither index above starts
-- with: both lead on `rule_id`, and an invite has none.
CREATE INDEX mail_send_by_task ON mail_send(task_id);

-- What the mail engine decided, for every rule and every event, not just the
-- mails it sent.
--
-- `mail_send` only exists once a mail is owed and claimed; a card that moved
-- but matched no rule, a rule that was disabled, a task deleted before its
-- mail could go out — none of that left a trace. This is that trace, so "why
-- did nobody get mailed" has an answer that is not "look at the logs from
-- that day".
--
-- `task_id` carries no foreign key on purpose: a `task_gone` row exists
-- precisely because the task it names no longer does, and it has to keep
-- saying so afterwards.
CREATE TABLE mail_decision (
    id         TEXT PRIMARY KEY,
    rule_id    TEXT NOT NULL REFERENCES mail_rule(id) ON DELETE CASCADE,
    event_id   TEXT NOT NULL,
    task_id    TEXT NOT NULL,
    outcome    TEXT NOT NULL CHECK (outcome IN
        ('owed', 'already_owed', 'no_recipients', 'not_matched', 'disabled', 'task_gone')),
    detail     TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL
);
CREATE UNIQUE INDEX mail_decision_once ON mail_decision(rule_id, event_id, task_id);
CREATE INDEX mail_decision_recent ON mail_decision(created_at);
CREATE INDEX mail_decision_by_task ON mail_decision(task_id);
