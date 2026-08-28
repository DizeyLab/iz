-- A profile picture lives in the database as bytes, the way an attachment
-- lives in its file (0010): no filesystem path survives a backup or a move.
-- `photo_path` (0001) never carried a value and is left behind, dead.
ALTER TABLE user ADD COLUMN photo BLOB;
ALTER TABLE user ADD COLUMN photo_mime TEXT;
