-- The four security knobs as workspace settings.
--
-- Until this change the attempt allowance, the rate-limit window, the session
-- lifetime and the sign-in-link lifetime were named consts in `accounts.rs`;
-- they are policy, and policy belongs to the admin on the Settings screen, the
-- way the attachment limits already did. The `DEFAULT` on each column is the
-- backfill: SQLite fills every existing row with it as the column is added.
-- Databases already carrying the columns never see this file applied piecemeal
-- — they reach the declared shape through `iz reconcile`, whose copy map
-- backfills the same numbers.
--
-- The numbers here mirror `store::DEFAULT_RATE_LIMIT_ATTEMPTS` and its three
-- siblings in `crates/iz-core/src/store/mod.rs` — SQL cannot read a Rust
-- const, so the two lists must be changed together.

ALTER TABLE workspace ADD COLUMN rate_limit_attempts       INTEGER NOT NULL DEFAULT 10;
ALTER TABLE workspace ADD COLUMN rate_window_minutes       INTEGER NOT NULL DEFAULT 15;
ALTER TABLE workspace ADD COLUMN session_lifetime_days     INTEGER NOT NULL DEFAULT 14;
ALTER TABLE workspace ADD COLUMN signin_link_lifetime_days INTEGER NOT NULL DEFAULT 7;
