-- The first-sign-in screen greets the invited person with the name of whoever
-- made the account. Nothing recorded that, so the screen was reading the
-- invitee's own name back to them: "Kay Watcher made you an account".
--
-- The column is nullable and stays null for the first account, which nobody
-- invited. It references user(id) rather than copying a name, so a later
-- rename is reflected rather than frozen at invite time.
ALTER TABLE user ADD COLUMN invited_by TEXT REFERENCES user(id);
