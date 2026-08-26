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

-- `mail_send.event_id` pointed at `transition(id)` alone. A freeing is an event
-- too, so the column names one of two tables and the foreign key has to go.
-- SQLite cannot drop a constraint in place, so the table is rebuilt and its
-- rows are carried over.
CREATE TABLE mail_send_next (
    id              TEXT PRIMARY KEY,
    rule_id         TEXT NOT NULL REFERENCES mail_rule(id) ON DELETE CASCADE,
    -- The transition or the freeing that caused it. Which table it is in is
    -- answered by looking, and a send is rebuilt from whichever one has it.
    event_id        TEXT NOT NULL,
    task_id         TEXT NOT NULL REFERENCES task(id),
    recipient       TEXT NOT NULL,
    state           TEXT NOT NULL CHECK (state IN ('pending', 'sent', 'failed', 'abandoned')),
    attempts        INTEGER NOT NULL DEFAULT 0,
    last_error      TEXT,
    claimed_at      TEXT NOT NULL,
    next_attempt_at TEXT,
    sent_at         TEXT
);

INSERT INTO mail_send_next
SELECT id, rule_id, event_id, task_id, recipient, state, attempts, last_error,
       claimed_at, next_attempt_at, sent_at
FROM mail_send;

DROP INDEX IF EXISTS mail_send_once;
DROP INDEX IF EXISTS mail_send_owed;
DROP INDEX IF EXISTS mail_send_by_rule;
DROP TABLE mail_send;
ALTER TABLE mail_send_next RENAME TO mail_send;

CREATE UNIQUE INDEX mail_send_once ON mail_send(rule_id, event_id, task_id, recipient);
CREATE INDEX mail_send_owed ON mail_send(next_attempt_at);
CREATE INDEX mail_send_by_rule ON mail_send(rule_id, sent_at);
