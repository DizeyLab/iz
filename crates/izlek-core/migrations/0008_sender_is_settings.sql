-- The sender comes back into the table, because the admin sets it from the
-- Settings screen and not from the server's environment.
--
-- Migration 0006 moved these columns out on the argument that a password in a
-- row is a password a handler can be made to return. That argument is sound
-- and the mitigations survive: the password has no field on `Workspace`, so
-- no query that loads a workspace can carry it; it is written by
-- `Store::set_sender` and read by `Store::smtp_password`, which only the
-- mailer calls; and nothing that reaches a page ever holds it.
--
-- What it cost was the artboard's own screen: `SMTP HOST`, `PORT`,
-- `USERNAME`, `PASSWORD`, `FROM NAME`, `FROM ADDRESS` and a Save button, with
-- the sender changed by whoever runs the workspace rather than by whoever has
-- a shell on the box. A three-person team has those be different people, and
-- the second one is not always awake.
--
-- The password is stored as it is typed. It has to be: the server presents it
-- to the mail host on every send, so a one-way hash would make sending
-- impossible, and encrypting it under a key that also lives on this disk moves
-- the problem without shrinking it. The consequence is stated rather than
-- dressed up — whoever holds a copy of this file holds the sender's password,
-- so it belongs to a sending account and to nothing else.
--
-- Added rather than rebuilt: `ALTER TABLE ... ADD COLUMN` works in place, and
-- every one of these is nullable, which is what a workspace with no sender is.
ALTER TABLE workspace ADD COLUMN smtp_host         TEXT;
ALTER TABLE workspace ADD COLUMN smtp_port         INTEGER;
ALTER TABLE workspace ADD COLUMN smtp_username     TEXT;
ALTER TABLE workspace ADD COLUMN smtp_password     TEXT;
ALTER TABLE workspace ADD COLUMN smtp_from_name    TEXT;
ALTER TABLE workspace ADD COLUMN smtp_from_address TEXT;
