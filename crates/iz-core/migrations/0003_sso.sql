-- SSO: identity moves to im.
--
-- The user table loses everything the password era kept: the hash, the
-- inviter, the photo marker and the feed marker. It gains `oidc_sub` — who
-- im says the row is, nullable because no existing row has met the provider
-- yet (the first sign-in claims by address) — and `disabled`, so an admin
-- can take an account out of sign-in without deleting its history.
--
-- The session, sign-in-link and rate-limit ledgers go with the flows they
-- served: per-request introspection replaces sessions, and there is no
-- password left to guess. The workspace's four security knobs go the same
-- way — policy the provider now owns.
--
-- An empty database is built by applying the migrations in order, so this
-- file transforms exactly what 0001+0002 built; a live database crosses
-- over through `reconcile`'s copy map instead, which lands on the same
-- shape.

ALTER TABLE user ADD COLUMN oidc_sub TEXT;

ALTER TABLE user ADD COLUMN disabled INTEGER NOT NULL DEFAULT 0;

DROP TABLE session;
DROP TABLE signin_link;
DROP TABLE auth_attempt;

-- The password era's columns leave by rebuilding the table: `invited_by`
-- carries a REFERENCES clause, and SQLite will not DROP a column named in
-- a foreign key. The surviving columns — and any rows on a live database
-- this ever meets — cross over by name; the two new columns ride along.
CREATE TABLE user_new (
    id                TEXT PRIMARY KEY,
    workspace_id      TEXT NOT NULL REFERENCES workspace(id),
    oidc_sub          TEXT,
    email             TEXT NOT NULL,
    display_name      TEXT NOT NULL,
    role              TEXT NOT NULL,
    disabled          INTEGER NOT NULL DEFAULT 0,
    timezone          TEXT NOT NULL DEFAULT 'UTC',
    theme             TEXT NOT NULL DEFAULT 'light',
    language          TEXT NOT NULL DEFAULT 'en',
    ui                TEXT NOT NULL DEFAULT 'instrument',
    created_at        TEXT NOT NULL,
    last_signed_in_at TEXT
);
INSERT INTO user_new (id, workspace_id, oidc_sub, email, display_name, role,
    disabled, timezone, theme, language, ui, created_at, last_signed_in_at)
    SELECT id, workspace_id, oidc_sub, email, display_name, role, disabled,
    timezone, theme, language, ui, created_at, last_signed_in_at FROM user;
DROP TABLE user;
ALTER TABLE user_new RENAME TO user;
CREATE UNIQUE INDEX user_email_unique ON user(workspace_id, email);
CREATE UNIQUE INDEX user_oidc_sub_unique ON user(oidc_sub) WHERE oidc_sub IS NOT NULL;

ALTER TABLE workspace DROP COLUMN rate_limit_attempts;
ALTER TABLE workspace DROP COLUMN rate_window_minutes;
ALTER TABLE workspace DROP COLUMN session_lifetime_days;
ALTER TABLE workspace DROP COLUMN signin_link_lifetime_days;
