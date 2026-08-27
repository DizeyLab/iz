-- A task created directly into a column never wrote a transition, so a rule
-- watching that column ("When status becomes In Progress") never fired for
-- it — only a later move into the column did. `create_task` now writes a
-- transition in the same statement as the insert, with `from_column` set to
-- '' (empty string) meaning "created into", never a real column.
--
-- `from_column` carried `REFERENCES board_column(id)`, and no column's id is
-- ever ''. SQLite cannot drop a column's foreign key in place, so the table
-- is rebuilt, same shape as 0005_freeing.sql and 0013.
PRAGMA foreign_keys = OFF;

CREATE TABLE transition_next (
    id          TEXT PRIMARY KEY,
    task_id     TEXT NOT NULL REFERENCES task(id),
    -- No REFERENCES: '' means "created into", and is never a column id.
    from_column TEXT NOT NULL,
    to_column   TEXT NOT NULL REFERENCES board_column(id),
    actor_id    TEXT NOT NULL REFERENCES user(id),
    created_at  TEXT NOT NULL
);

INSERT INTO transition_next
SELECT id, task_id, from_column, to_column, actor_id, created_at
FROM transition;

DROP INDEX IF EXISTS transition_by_task;
DROP TABLE transition;
ALTER TABLE transition_next RENAME TO transition;

CREATE INDEX transition_by_task ON transition(task_id, created_at);

PRAGMA foreign_keys = ON;
