-- What the mail engine decided, for every rule and every event, not just the
-- mails it sent.
--
-- `mail_send` only exists once a mail is owed and claimed; a card that moved
-- but matched no rule, a rule that was disabled, a task deleted before its
-- mail could go out — none of that left a trace. This table is that trace:
-- one row per (rule, event, task), win or not, so "why did nobody get
-- mailed" has an answer that is not "look at the logs from that day".
--
-- `event_id` names a transition or a freeing the same way `mail_send.event_id`
-- does and for the same reason (0005_freeing.sql:23-26): two tables can cause
-- a decision, so the column cannot point a foreign key at either one and
-- still hold both. `task_id` carries no foreign key either, on purpose: a
-- `task_gone` row exists precisely because the task it names no longer does,
-- and it has to keep saying so after the row it would have pointed at is
-- deleted.
CREATE TABLE mail_decision (
    id         TEXT PRIMARY KEY,
    rule_id    TEXT NOT NULL REFERENCES mail_rule(id) ON DELETE CASCADE,
    event_id   TEXT NOT NULL,
    task_id    TEXT NOT NULL,
    -- 'owed'           — a send was claimed for this
    -- 'already_owed'   — a send already existed; this is a replay
    -- 'no_recipients'  — the audience resolved to nobody
    -- 'not_matched'    — the event did not fire this rule
    -- 'disabled'       — the rule was off when the event happened
    -- 'task_gone'      — the task named by the event is deleted
    outcome    TEXT NOT NULL CHECK (outcome IN
        ('owed', 'already_owed', 'no_recipients', 'not_matched', 'disabled', 'task_gone')),
    detail     TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL
);

-- Same event, same rule, same task decided twice is a bug worth refusing at
-- the schema rather than finding in a report: a retry has to land on the
-- decision that is already there, not beside it.
CREATE UNIQUE INDEX mail_decision_once ON mail_decision(rule_id, event_id, task_id);
CREATE INDEX mail_decision_recent ON mail_decision(created_at);

-- The activity strip's own index was never added when the table was: reading
-- the most recent activity has been a full scan since 0001.
CREATE INDEX activity_recent ON activity(created_at);
