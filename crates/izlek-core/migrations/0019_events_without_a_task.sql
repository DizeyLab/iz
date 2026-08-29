-- Account and admin events — sign-ins, invites, role changes — have no task
-- to sit under, and `activity.task_id` being NOT NULL left them unloggable.
-- The table is rebuilt with the task column nullable.
CREATE TABLE activity_next (
    id         TEXT PRIMARY KEY,
    task_id    TEXT REFERENCES task(id),
    actor_id   TEXT REFERENCES user(id),
    kind       TEXT NOT NULL,
    detail     TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL
);

INSERT INTO activity_next
SELECT id, task_id, actor_id, kind, detail, created_at
FROM activity;

DROP INDEX IF EXISTS activity_by_task;
DROP INDEX IF EXISTS activity_recent;
DROP TABLE activity;
ALTER TABLE activity_next RENAME TO activity;

CREATE INDEX activity_by_task ON activity(task_id);
CREATE INDEX activity_recent ON activity(created_at);
