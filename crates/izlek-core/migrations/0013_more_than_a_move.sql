-- The rule table named its two triggers in a CHECK: 'status' or 'unblocked',
-- nothing else. Every new trigger has meant a migration just to widen that
-- list. This rebuild drops the CHECK on `trigger_kind` entirely — a future
-- trigger is a value, not a schema change — and widens `audience` to add
-- 'creator' the same way 0004_mail.sql wrote it in the first place.
--
-- SQLite cannot drop or relax a CHECK in place, so the table is rebuilt,
-- same shape of migration as 0005_freeing.sql and 0012.
--
-- `mail_send.rule_id` and `mail_decision.rule_id` both point at `mail_rule`
-- with ON DELETE CASCADE, and every connection this store opens runs with
-- foreign keys on. SQLite performs an implicit DELETE FROM before a DROP
-- TABLE takes effect, and that implicit delete walks ON DELETE CASCADE like
-- any other — dropping `mail_rule` with rows in it would carry every send and
-- every decision away with it. `PRAGMA foreign_keys` is documented as a
-- no-op inside a transaction, which is where every migration in this file
-- runs, so the pragma is set here anyway and then proven by the reopen test
-- rather than trusted from the manual.
PRAGMA foreign_keys = OFF;

CREATE TABLE mail_rule_next (
    id             TEXT PRIMARY KEY,
    board_id       TEXT NOT NULL REFERENCES board(id),
    -- No CHECK list: the set of triggers grows without a schema change now.
    trigger_kind   TEXT NOT NULL,
    trigger_column TEXT REFERENCES board_column(id),
    subject        TEXT NOT NULL,
    -- 'assignees' — the people the card points at; 'board' — everyone who can
    -- write on the board; 'creator' — whoever opened the card.
    audience       TEXT NOT NULL CHECK (audience IN ('assignees', 'board', 'creator')),
    enabled        INTEGER NOT NULL DEFAULT 1,
    created_at     TEXT NOT NULL,
    -- A status rule names a column and an unblocked rule does not. The check
    -- is here so a half-written rule cannot be stored at all.
    CHECK ((trigger_kind = 'status') = (trigger_column IS NOT NULL))
);

INSERT INTO mail_rule_next
SELECT id, board_id, trigger_kind, trigger_column, subject, audience, enabled, created_at
FROM mail_rule;

DROP INDEX IF EXISTS mail_rule_by_board;
DROP TABLE mail_rule;
ALTER TABLE mail_rule_next RENAME TO mail_rule;

CREATE INDEX mail_rule_by_board ON mail_rule(board_id);

PRAGMA foreign_keys = ON;
