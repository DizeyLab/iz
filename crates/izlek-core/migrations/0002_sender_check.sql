-- Whether the mail server let us in, which is a different fact from whether a
-- mail was delivered.
--
-- `smtp_test_*` records a real message going out to a real inbox. This records
-- a handshake: connect, negotiate TLS, say hello, authenticate, hang up. It
-- proves the host, the port, the encryption and the credentials, and it proves
-- nothing about whether the from-address is one this account may send as — a
-- server that accepts the login can still refuse the envelope. Kept apart so
-- neither can be read as the other.
--
-- Cleared whenever the sender is edited, for the same reason the test result
-- is: it was about settings that no longer exist.
ALTER TABLE workspace ADD COLUMN smtp_check_at    TEXT;
ALTER TABLE workspace ADD COLUMN smtp_check_ms    INTEGER;
ALTER TABLE workspace ADD COLUMN smtp_check_error TEXT;
