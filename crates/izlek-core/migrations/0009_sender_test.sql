-- The result of the last "send test mail to myself", kept on the workspace.
--
-- It is kept rather than shown once because of what the panel is for: an admin
-- fills the sender in, presses the button, and comes back tomorrow wondering
-- whether it ever worked. A result that vanished on reload would answer that
-- question with silence, and silence is what the whole mail ledger exists to
-- avoid.
--
-- `smtp_test_error` is the mail server's own words, kept so the admin can act
-- on them. Nothing writes the password into it: the mailer builds its errors
-- from what the server said, never from the credentials it sent.
ALTER TABLE workspace ADD COLUMN smtp_test_at    INTEGER;
ALTER TABLE workspace ADD COLUMN smtp_test_ms    INTEGER;
ALTER TABLE workspace ADD COLUMN smtp_test_error TEXT;
