-- Mail rules and the ledger of what they sent.
--
-- 0001 sketched both tables before there was an engine to read them, and the
-- sketch is not the shape the engine needs: a rule needs a column id rather
-- than a free-text trigger value, and a send needs a recipient, a state and an
-- attempt count so a mail that failed can be tried again and seen. Nothing has
-- ever written a row to either table -- no store method touched them until
-- this migration -- so they are rebuilt rather than migrated in place.
DROP INDEX IF EXISTS mail_send_once;
DROP TABLE IF EXISTS mail_send;
DROP TABLE IF EXISTS mail_rule;

CREATE TABLE mail_rule (
    id             TEXT PRIMARY KEY,
    board_id       TEXT NOT NULL REFERENCES board(id),
    -- 'status' fires when a card crosses into `trigger_column`; 'unblocked'
    -- fires for a task whose last blocker just finished.
    trigger_kind   TEXT NOT NULL CHECK (trigger_kind IN ('status', 'unblocked')),
    trigger_column TEXT REFERENCES board_column(id),
    subject        TEXT NOT NULL,
    -- 'assignees' — the people the card points at; 'board' — everyone who can
    -- write on the board. A Viewer is in neither: a Viewer is never mailed.
    audience       TEXT NOT NULL CHECK (audience IN ('assignees', 'board')),
    enabled        INTEGER NOT NULL DEFAULT 1,
    created_at     TEXT NOT NULL,
    -- A status rule names a column and an unblocked rule does not. The check
    -- is here so a half-written rule cannot be stored at all.
    CHECK ((trigger_kind = 'status') = (trigger_column IS NOT NULL))
);

CREATE INDEX mail_rule_by_board ON mail_rule(board_id);

-- One row per (rule, event, task, recipient). The row is written BEFORE the
-- mail is handed to the server, and the unique index below is what decides
-- whether this process owns the send: an insert that conflicts means somebody
-- else already owns it, and this process sends nothing. A check-then-insert
-- would let the engine running twice mail the same person twice.
--
-- `event_id` is the transition that caused the send. It is what the engine calls
-- of "the same happening", so a transition replayed on a restart lands on the
-- rows that already exist rather than on new ones.
CREATE TABLE mail_send (
    id              TEXT PRIMARY KEY,
    rule_id         TEXT NOT NULL REFERENCES mail_rule(id) ON DELETE CASCADE,
    event_id        TEXT NOT NULL REFERENCES transition(id),
    task_id         TEXT NOT NULL REFERENCES task(id),
    recipient       TEXT NOT NULL,
    -- 'pending'   — owned, not yet accepted by the server
    -- 'sent'      — the server took it
    -- 'failed'    — the server refused in a way worth trying again
    -- 'abandoned' — refused in a way that will not change, or out of attempts
    state           TEXT NOT NULL CHECK (state IN ('pending', 'sent', 'failed', 'abandoned')),
    attempts        INTEGER NOT NULL DEFAULT 0,
    last_error      TEXT,
    claimed_at      TEXT NOT NULL,
    -- When a failed send may be tried again. NULL once it is sent or given up
    -- on, so "what is owed" is one indexed read.
    next_attempt_at TEXT,
    sent_at         TEXT
);

CREATE UNIQUE INDEX mail_send_once ON mail_send(rule_id, event_id, task_id, recipient);
CREATE INDEX mail_send_owed ON mail_send(next_attempt_at);
CREATE INDEX mail_send_by_rule ON mail_send(rule_id, sent_at);
