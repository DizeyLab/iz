-- No photo limit: avatars proxy from im, and nothing else ever capped a
-- photo, so the workspace stops carrying a ceiling nothing enforces.
--
-- An empty database is built by applying the migrations in order, so this
-- file drops exactly what 0001 created; a live database crosses over
-- through `reconcile`'s copy map instead, which simply leaves the column
-- behind.
ALTER TABLE workspace DROP COLUMN photo_limit_bytes;
