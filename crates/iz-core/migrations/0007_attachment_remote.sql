-- An attachment row learns where its bytes live.
--
-- `local`: under `<storage>/attachments/<id>`, as every row before this
-- column kept them. `stored`: through the family's Files service, keyed by
-- the row's own id — in's service routes are keyed by external id, which is
-- this id, so no second id column is added. Rows from before the column are
-- local by definition and start there; nothing is backfilled.
ALTER TABLE attachment ADD COLUMN remote_state TEXT NOT NULL DEFAULT 'local';
