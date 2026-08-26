-- Dizey initial schema.
--
-- Plain versioned SQL applied at boot: Turso has no migration runner of its
-- own, and keeping the schema as SQL is what makes the store trait swappable.
-- Timestamps are RFC 3339 text in UTC. Ids are UUIDs as text.

CREATE TABLE workspace (
    id                     TEXT PRIMARY KEY,
    name                   TEXT NOT NULL,
    created_at             TEXT NOT NULL,
    -- The single sender every mail in the workspace goes through. Written by
    -- the admin in Settings; the password is never returned to the client.
    smtp_host              TEXT,
    smtp_port              INTEGER,
    smtp_username          TEXT,
    smtp_password          TEXT,
    smtp_from_name         TEXT,
    smtp_from_address      TEXT,
    -- Limits. Each states its consequence in the UI, not here.
    attachment_limit_bytes INTEGER NOT NULL DEFAULT 26214400,
    photo_limit_bytes      INTEGER NOT NULL DEFAULT 2097152,
    -- JSON array of allowed extensions. An empty array means every type.
    allowed_file_types     TEXT NOT NULL DEFAULT '[]'
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
    photo_path        TEXT,
    created_at        TEXT NOT NULL,
    last_signed_in_at TEXT
);
CREATE UNIQUE INDEX user_email_unique ON user(workspace_id, email);

-- First sign-in / resend links. Only the hash is stored: the plaintext is shown
-- once when the admin creates or resends it.
CREATE TABLE signin_link (
    id         TEXT PRIMARY KEY,
    user_id    TEXT NOT NULL REFERENCES user(id),
    token_hash TEXT NOT NULL,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    used_at    TEXT
);
CREATE UNIQUE INDEX signin_link_token ON signin_link(token_hash);

CREATE TABLE board (
    id           TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL REFERENCES workspace(id),
    name         TEXT NOT NULL,
    task_prefix  TEXT NOT NULL DEFAULT 'DZ',
    next_task_no INTEGER NOT NULL DEFAULT 1,
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

CREATE TABLE task (
    id          TEXT PRIMARY KEY,
    board_id    TEXT NOT NULL REFERENCES board(id),
    -- Human key from the mockups, e.g. DZ-14.
    task_key    TEXT NOT NULL,
    title       TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    column_id   TEXT NOT NULL REFERENCES board_column(id),
    deadline    TEXT,
    position    REAL NOT NULL DEFAULT 0,
    created_by  TEXT NOT NULL REFERENCES user(id),
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    done_at     TEXT,
    deleted_at  TEXT
);
CREATE UNIQUE INDEX task_key_unique ON task(board_id, task_key);
CREATE INDEX task_by_column ON task(column_id);

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
CREATE TABLE task_dependency (
    blocked_task_id  TEXT NOT NULL REFERENCES task(id),
    blocking_task_id TEXT NOT NULL REFERENCES task(id),
    created_at       TEXT NOT NULL,
    cleared_at       TEXT,
    PRIMARY KEY (blocked_task_id, blocking_task_id)
);

CREATE TABLE comment (
    id         TEXT PRIMARY KEY,
    task_id    TEXT NOT NULL REFERENCES task(id),
    author_id  TEXT NOT NULL REFERENCES user(id),
    body       TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX comment_by_task ON comment(task_id);

CREATE TABLE attachment (
    id          TEXT PRIMARY KEY,
    task_id     TEXT NOT NULL REFERENCES task(id),
    comment_id  TEXT REFERENCES comment(id),
    file_name   TEXT NOT NULL,
    mime_type   TEXT NOT NULL,
    size_bytes  INTEGER NOT NULL,
    stored_path TEXT NOT NULL,
    uploaded_by TEXT NOT NULL REFERENCES user(id),
    created_at  TEXT NOT NULL
);

-- The activity strip in the task detail: who did what, and whether it mailed.
CREATE TABLE activity (
    id         TEXT PRIMARY KEY,
    task_id    TEXT NOT NULL REFERENCES task(id),
    actor_id   TEXT REFERENCES user(id),
    kind       TEXT NOT NULL,
    detail     TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL
);
CREATE INDEX activity_by_task ON activity(task_id);

-- Mail rules are written only by the admin and are always on. A rule that
-- addresses an external address stays off until that address confirms.
CREATE TABLE mail_rule (
    id               TEXT PRIMARY KEY,
    workspace_id     TEXT NOT NULL REFERENCES workspace(id),
    board_id         TEXT REFERENCES board(id),
    -- 'status_becomes' | 'deadline_in' | 'unblocked'
    trigger_kind     TEXT NOT NULL,
    trigger_value    TEXT NOT NULL,
    subject          TEXT NOT NULL,
    -- 'assignees' | 'assignees_watchers' | 'everyone' | 'external'
    audience         TEXT NOT NULL,
    external_address TEXT,
    external_confirmed INTEGER NOT NULL DEFAULT 0,
    created_at       TEXT NOT NULL,
    last_sent_at     TEXT
);

-- One row per (rule, task, transition) actually sent, so a revert-then-redo
-- does not mail twice.
CREATE TABLE mail_send (
    id         TEXT PRIMARY KEY,
    rule_id    TEXT NOT NULL REFERENCES mail_rule(id),
    task_id    TEXT NOT NULL REFERENCES task(id),
    transition TEXT NOT NULL,
    sent_at    TEXT NOT NULL
);
CREATE UNIQUE INDEX mail_send_once ON mail_send(rule_id, task_id, transition);

-- Read-only links: dizey.sh/v/<id>. Only the hash of the token is stored, so a
-- database copy does not hand out working links; revoking is server-side and
-- immediate.
CREATE TABLE view_link (
    id           TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL REFERENCES workspace(id),
    board_id     TEXT NOT NULL REFERENCES board(id),
    token_hash   TEXT NOT NULL,
    -- 'whole_board' | 'deadlines'
    scope        TEXT NOT NULL,
    created_by   TEXT NOT NULL REFERENCES user(id),
    created_at   TEXT NOT NULL,
    expires_at   TEXT,
    revoked_at   TEXT,
    open_count   INTEGER NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX view_link_token ON view_link(token_hash);
