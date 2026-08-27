-- Per-user display preferences. Stored data everywhere else stays UTC and
-- language-neutral; these three columns only ever change how a browser
-- renders that data for the one person signed in as this user.
ALTER TABLE user ADD COLUMN timezone TEXT NOT NULL DEFAULT 'UTC';
ALTER TABLE user ADD COLUMN theme    TEXT NOT NULL DEFAULT 'light';
ALTER TABLE user ADD COLUMN language TEXT NOT NULL DEFAULT 'en';
