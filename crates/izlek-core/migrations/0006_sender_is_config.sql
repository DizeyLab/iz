-- The sender moves out of the database entirely.
--
-- `workspace` carried six `smtp_*` columns, one of them the password. A
-- password in a row is a password an admin session can ask for: any handler
-- that can read the workspace record is one bug away from returning it, and a
-- database file that is copied for a backup carries it along. The sender is
-- infrastructure, not workspace content — it is the same for everyone in the
-- workspace, it changes when the deployment changes, and it is already read
-- from `IZLEK_SMTP_HOST`, `IZLEK_SMTP_PORT`, `IZLEK_SMTP_USERNAME`,
-- `IZLEK_SMTP_PASSWORD` and `IZLEK_MAIL_FROM` at boot.
--
-- So the columns go. Settings shows what the process is configured with and
-- says the password lives in the server's configuration; changing it means
-- changing the environment variable and restarting.
--
-- The limits stay: they are workspace content, an admin edits them, and no
-- restart should be needed to raise an attachment ceiling.
--
-- SQLite cannot drop several columns and their comments in place here, so the
-- table is rebuilt. Other tables reference `workspace` by name, and the name
-- is what the rename restores.
CREATE TABLE workspace_next (
    id                     TEXT PRIMARY KEY,
    name                   TEXT NOT NULL,
    created_at             TEXT NOT NULL,
    attachment_limit_bytes INTEGER NOT NULL DEFAULT 26214400,
    photo_limit_bytes      INTEGER NOT NULL DEFAULT 2097152,
    -- JSON array of allowed extensions. An empty array means every type.
    allowed_file_types     TEXT NOT NULL DEFAULT '[]'
);

INSERT INTO workspace_next
SELECT id, name, created_at, attachment_limit_bytes, photo_limit_bytes, allowed_file_types
FROM workspace;

DROP TABLE workspace;
ALTER TABLE workspace_next RENAME TO workspace;
