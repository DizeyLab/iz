-- Attachments, moved off the filesystem and into the database file.
--
-- The table 0001 wrote kept a `stored_path`, and nothing ever filled it in.
-- A path is the wrong shape for two reasons. Izlek is one process over one
-- database file, and a second place to keep state is a second thing to back
-- up, to lock, and to get out of step with the rows that name it. And a path
-- is exactly where an uploaded file name must never end up: `../../etc` is a
-- valid file name and a terrible path. Keeping the bytes here means the name
-- is only ever a label printed on a chip, never anything the server resolves.
--
-- Nothing has ever been written to the old table, so it goes rather than
-- being migrated.
DROP TABLE attachment;

CREATE TABLE attachment (
    id          TEXT PRIMARY KEY,
    task_id     TEXT NOT NULL REFERENCES task(id),
    -- The comment this file was posted with, when it was posted with one.
    comment_id  TEXT REFERENCES comment(id),
    -- What the browser called it, kept for display only.
    file_name   TEXT NOT NULL,
    -- What the server decided it is, from the bytes. Never what the upload
    -- claimed.
    mime_type   TEXT NOT NULL,
    size_bytes  INTEGER NOT NULL,
    bytes       BLOB NOT NULL,
    uploaded_by TEXT NOT NULL REFERENCES user(id),
    created_at  TEXT NOT NULL
);
CREATE INDEX attachment_by_task ON attachment(task_id);
