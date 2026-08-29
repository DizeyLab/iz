-- A notice is an admin's own mail to a member — same shape as an invite:
-- no rule, no event, no task, just a subject and a body. 0012's CHECK only
-- named 'invite' alongside 'rule', so a 'notice' row would fail it. SQLite
-- cannot widen a CHECK in place, so the table is rebuilt again, same shape
-- of migration as 0012/0013.
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
    kind            TEXT NOT NULL DEFAULT 'rule' CHECK (kind IN ('rule', 'invite', 'notice')),
    subject         TEXT,
    body            TEXT,
    CHECK ((kind = 'rule') = (rule_id IS NOT NULL AND event_id IS NOT NULL AND task_id IS NOT NULL)),
    CHECK ((kind IN ('invite', 'notice')) = (subject IS NOT NULL AND body IS NOT NULL))
);

INSERT INTO mail_send_next
SELECT id, rule_id, event_id, task_id, recipient, state, attempts, last_error,
       claimed_at, next_attempt_at, sent_at, kind, subject, body
FROM mail_send;

DROP INDEX IF EXISTS mail_send_once;
DROP INDEX IF EXISTS mail_send_owed;
DROP INDEX IF EXISTS mail_send_by_rule;
DROP TABLE mail_send;
ALTER TABLE mail_send_next RENAME TO mail_send;

CREATE UNIQUE INDEX mail_send_once ON mail_send(rule_id, event_id, task_id, recipient);
CREATE INDEX mail_send_owed ON mail_send(next_attempt_at);
CREATE INDEX mail_send_by_rule ON mail_send(rule_id, sent_at);
