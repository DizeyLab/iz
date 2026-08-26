-- Sessions, the single-owner claim, and the rate-limit ledger.

-- Exactly one row, ever. The primary key is the whole point: two requests
-- racing to claim an empty workspace both try to insert singleton = 1 and the
-- database picks the winner, inside the same transaction that writes the
-- workspace and its admin.
CREATE TABLE workspace_owner (
    singleton  INTEGER PRIMARY KEY,
    user_id    TEXT NOT NULL REFERENCES user(id),
    claimed_at TEXT NOT NULL
);

-- A signed-in browser. Only the hash of the cookie token is stored, exactly
-- like sign-in links and read-only links.
CREATE TABLE session (
    id         TEXT PRIMARY KEY,
    user_id    TEXT NOT NULL REFERENCES user(id),
    token_hash TEXT NOT NULL,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    revoked_at TEXT
);
CREATE UNIQUE INDEX session_token ON session(token_hash);
CREATE INDEX session_by_user ON session(user_id);

-- Sign-in attempts and link redemptions, one row per attempt, counted over a
-- window. The bucket is the address or the client address, never the password.
CREATE TABLE auth_attempt (
    id        TEXT PRIMARY KEY,
    bucket    TEXT NOT NULL,
    attempted_at TEXT NOT NULL
);
CREATE INDEX auth_attempt_by_bucket ON auth_attempt(bucket, attempted_at);
