-- Every move of a card from one column to another, written as a fact in the
-- same transaction as the move itself.
--
-- The mail rules trigger on the transition, not on the task's current column:
-- a card that goes Review -> Done -> Review has crossed into Done once, and
-- nothing recomputed from `task.column_id` afterwards can tell you that. So
-- this table is the record, and the rules read it.
CREATE TABLE transition (
    id          TEXT PRIMARY KEY,
    task_id     TEXT NOT NULL REFERENCES task(id),
    from_column TEXT NOT NULL REFERENCES board_column(id),
    to_column   TEXT NOT NULL REFERENCES board_column(id),
    actor_id    TEXT NOT NULL REFERENCES user(id),
    created_at  TEXT NOT NULL
);

CREATE INDEX transition_by_task ON transition(task_id, created_at);
