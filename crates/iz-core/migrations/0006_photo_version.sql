-- The face wears a version now. im counts every change to a member's photo,
-- and the count rides the directory: a sync that sees a new number knows the
-- face changed without fetching it, and an avatar URL stamped with the number
-- a page rendered can be cached hard — the browser only refetches when the
-- stamp it holds is no longer the stamp the row carries. Rows from before
-- this column start at zero, the count of a member im has never photographed.
ALTER TABLE user ADD COLUMN photo_version INTEGER NOT NULL DEFAULT 0;
