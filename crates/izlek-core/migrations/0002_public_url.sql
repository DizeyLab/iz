-- The address mail points at, when it is not the one the process was
-- configured with. A box behind a proxy answers on localhost and is reached
-- on a public name, and only an admin knows which — so this is workspace
-- content, edited in Settings, and `config/izlek.toml`'s `base_url` is what
-- it falls back to.
ALTER TABLE workspace ADD COLUMN public_url TEXT;
