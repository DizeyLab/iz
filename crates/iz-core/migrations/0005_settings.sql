-- App-level settings, one row per key.
--
-- The workspace row carries what members share; a key here is the
-- deployment's own and has no workspace to hang off. Two things live here
-- today, both written by the app itself rather than typed into a file: the
-- family list mirrored from im's `/family` (the topbar switcher renders
-- from it), and the public address a sign-out that started here sends the
-- browser back to.
--
-- A fresh table, so an old database simply gains it empty on rebuild;
-- `iz reconcile` maps it and copies nothing, because an older database
-- has no rows to give.
CREATE TABLE setting (
    key     TEXT PRIMARY KEY,
    value   TEXT NOT NULL
);
