-- Which interface this person uses. Display-only, as the preferences before
-- it: no stored data depends on it.
ALTER TABLE user ADD COLUMN ui TEXT NOT NULL DEFAULT 'instrument';
