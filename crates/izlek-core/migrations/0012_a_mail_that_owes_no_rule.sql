-- An invite is a mail with nobody it is owed to on the board and no event
-- that caused it — a person is invited before they have a task, a
-- transition, or a rule to owe them anything. `mail_send` has assumed all
-- three since 0004_mail.sql; this rebuild lets a row go without them.
--
-- `rule_id`/`event_id`/`task_id` become nullable, `rule_id`'s foreign key
-- kept for the rows that still have one. `kind` says which shape a row is,
-- and the two CHECKs make the shapes exclusive: a 'rule' send still has to
-- carry all three of its old columns, an 'invite' send has to carry the
-- subject and body a rule send gets from `mail_rule`/`compose` instead.
--
-- SQLite cannot loosen a column's NOT NULL or add a CHECK in place, so the
-- table is rebuilt again, same shape of migration as 0005_freeing.sql.
CREATE TABLE mail_send_next (
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
    kind            TEXT NOT NULL DEFAULT 'rule' CHECK (kind IN ('rule', 'invite')),
    subject         TEXT,
    body            TEXT,
    CHECK ((kind = 'rule') = (rule_id IS NOT NULL AND event_id IS NOT NULL AND task_id IS NOT NULL)),
    CHECK ((kind = 'invite') = (subject IS NOT NULL AND body IS NOT NULL))
);

INSERT INTO mail_send_next
SELECT id, rule_id, event_id, task_id, recipient, state, attempts, last_error,
       claimed_at, next_attempt_at, sent_at, 'rule', NULL, NULL
FROM mail_send;

DROP INDEX IF EXISTS mail_send_once;
DROP INDEX IF EXISTS mail_send_owed;
DROP INDEX IF EXISTS mail_send_by_rule;
DROP TABLE mail_send;
ALTER TABLE mail_send_next RENAME TO mail_send;

-- NULLs are distinct in a SQLite unique index, so two invite rows (whose
-- rule_id/event_id/task_id are all NULL) never collide here — each invite
-- send is its own row, which is intended: a resend is a new mail, not a
-- replay of the same one.
CREATE UNIQUE INDEX mail_send_once ON mail_send(rule_id, event_id, task_id, recipient);
CREATE INDEX mail_send_owed ON mail_send(next_attempt_at);
CREATE INDEX mail_send_by_rule ON mail_send(rule_id, sent_at);
