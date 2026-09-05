//! Everything İz reads from `config/iz.toml`, in one place.
//!
//! Nothing here has a silent default once the file exists. A key that is
//! missing, empty or unusable stops the boot and says which key and which
//! file, because the alternative is worse than not starting: a wrong
//! `database` does not mean "no data", it means a second İz quietly
//! writing a different file while everyone believes they are looking at the
//! same board — and Turso is single-writer, so the two are not even
//! reconcilable afterwards.
//!
//! Development still needs to be one command, so the *absence* of
//! `config/iz.toml` is the opt-in that takes the development defaults: the
//! app writes the file itself, with those defaults in it and comments saying
//! what each key does, and then stops — the `[oidc]` keys have no default
//! worth guessing, so the first boot writes the file and says which keys to
//! fill in rather than starting half-authenticated. That is opt-in on purpose
//! too — it only ever happens once, because the second boot finds the file
//! it wrote the first time and reads it like any other.
//!
//! Whatever is finally resolved is printed once at startup — the database
//! path absolute, the address bound, the address mail will fall back to — so
//! "which file are we on" is answered by the log rather than by someone's
//! memory.
//!
//! The file is kept complete: a key it does not mention is appended, with its
//! comment and the default already in effect, so reading the file is the way
//! to learn what can be changed. A key nobody here knows is named in the
//! startup report rather than obeyed or refused — a typo should be visible
//! without being a boot failure, and a key this file has stopped honouring
//! must never half-configure anything.
//!
//! The sender is not here. Host, port, username, password and from-address
//! are workspace settings an admin writes on the Settings screen, so that
//! changing where mail goes out through does not need a shell on the box and
//! a restart.

use serde::Deserialize;
use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

/// The path, relative to the working directory, of the file `Config::load`
/// reads and, failing that, writes.
const FILE_NAME: &str = "config/iz.toml";

/// What a freshly written `config/iz.toml` says, comments included. Development
/// defaults, so that a plain `iz` in an empty directory is one command.
const DEVELOPMENT_DEFAULTS: &str = r#"# Where the one database file lives. One process holds it.
database = "iz.db"
# Where attachments live as files, created on boot.
storage = "storage"
# The address the server listens on. Environment variables are ignored —
# this is the only thing that decides where İz binds. It is also the
# address mail links fall back to, until an admin sets one in Settings.
listen = "127.0.0.1:7654"
# How long a live-update connection is held before the browser is asked to
# reconnect, in seconds. The reconnect is what re-checks the session, so a
# revoked sign-in stops receiving within this long.
live_seconds = 300
[oidc]
# The provider that signs people in. Required: no default is guessed.
issuer = ""
# The client id İz presents to the provider. Required.
client_id = ""
# The client secret İz presents to the provider. Required. The file holds a
# live credential once this is filled in, so it is created mode 0600.
client_secret = ""
# Where the provider sends the browser back after sign-in. Not written here:
# it defaults from `listen` on every boot, so moving the address never leaves
# a stale callback behind. Set it only when the public address differs from
# the bind address (a proxy in front, for instance).
# redirect_uri = "http://127.0.0.1:7654/auth/callback"
"#;

/// The default `listen` when the file is silent about it. A file missing this
/// key is completed with it on the next boot, so the silence lasts one run.
///
/// 7654 rather than a round number: 3000, 4000, 5000, 8000 and 8080 are what
/// every other thing on a developer's machine already took, and a first run
/// that dies on "address already in use" is a first run that teaches nothing.
/// This one is unassigned in IANA's registry and sits below the ephemeral
/// range, so the kernel never hands it to an outgoing connection either.
const DEFAULT_LISTEN: &str = "127.0.0.1:7654";

/// How long one live-update connection lasts before the server ends it and the
/// browser opens another. Five minutes is a compromise: a stream is
/// authenticated once, when it opens, so a session revoked mid-stream keeps
/// hearing until the next reconnect — and a reconnect costs one request, so
/// doing it every few seconds to shorten that window would be worse than the
/// window. Long enough to be cheap, short enough that a revoked session goes
/// quiet while the person who revoked it is still watching.
const DEFAULT_LIVE_SECONDS: u64 = 300;

/// Every optional key, with the comment and default a file missing it is
/// completed with. `database` is not here: it has no default worth guessing,
/// so its absence stops the boot instead.
const OPTIONAL_KEYS: &[(&str, &str)] = &[
    (
        "listen",
        concat!(
            "# The address the server listens on. Environment variables are ignored —\n",
            "# this is the only thing that decides where İz binds. It is also the\n",
            "# address mail links fall back to, until an admin sets one in Settings.\n",
            "listen = \"127.0.0.1:7654\"\n"
        ),
    ),
    (
        "live_seconds",
        concat!(
            "# How long a live-update connection is held before the browser is asked to\n",
            "# reconnect, in seconds. The reconnect is what re-checks the session, so a\n",
            "# revoked sign-in stops receiving within this long.\n",
            "live_seconds = 300\n"
        ),
    ),
];

/// The `[oidc]` table of `config/iz.toml`, before the values are checked.
/// Anything else the section says lands in `other`, which is read for its key
/// names only.
#[derive(Deserialize)]
struct OidcToml {
    issuer: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
    redirect_uri: Option<String>,
    #[serde(flatten, default)]
    other: std::collections::BTreeMap<String, toml::Value>,
}

/// The shape of `config/iz.toml`, before the values are checked. Anything
/// else the file says lands in `other`, which is read for its key names only
/// — enough for the report to say a key was seen and not obeyed.
#[derive(Deserialize)]
struct Toml {
    database: Option<String>,
    storage: Option<String>,
    listen: Option<String>,
    live_seconds: Option<u64>,
    oidc: Option<OidcToml>,
    #[serde(flatten)]
    other: std::collections::BTreeMap<String, toml::Value>,
}

/// The OIDC provider İz trusts, and how İz presents itself to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OidcConfig {
    /// The provider's issuer URL. Sign-ins from anywhere else are refused.
    pub issuer: String,
    /// The client id İz presents to the provider.
    pub client_id: String,
    /// The client secret İz presents to the provider. Never printed.
    pub client_secret: String,
    /// Where the provider sends the browser after sign-in. Defaults from
    /// `listen` when the file is silent about it.
    pub redirect_uri: String,
}

/// What the process needs to know before it opens a socket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// The database file, absolute. One process holds it.
    pub database: PathBuf,
    /// Where attachments and profile photos live as files, absolute. Binary
    /// never sits in the database: this directory and the database file are
    /// backed up together, or not at all.
    pub storage: PathBuf,
    /// The address the server binds. The only source for this — `HOST` and
    /// `PORT` environment variables are never read.
    pub listen: SocketAddr,
    /// Keys the file sets that nothing here reads, in the order the file
    /// gives them. Named at startup so a typo is visible.
    pub ignored: Vec<String>,
    /// How long one live-update connection is held open before the browser is
    /// asked to reconnect. The reconnect re-authenticates, which is how a
    /// session revoked mid-stream stops being fed.
    pub live_seconds: u64,
    /// The OIDC provider İz trusts.
    pub oidc: OidcConfig,
    /// Whether `config/iz.toml` did not exist and was just written with the
    /// development defaults this boot.
    pub defaulted: bool,
}

/// Why the process is not starting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// The file exists but is not valid TOML.
    Unparseable { why: String },
    /// A key is missing or set to an empty value.
    Missing(&'static str),
    /// A key is set to something the app cannot use.
    Invalid { key: &'static str, why: String },
    /// The file could not be read for a reason other than "it is not there"
    /// (permissions, for instance), or the default could not be written.
    Io(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Unparseable { why } => {
                write!(f, "not starting: {FILE_NAME} is not valid TOML — {why}")
            }
            ConfigError::Missing(key) => {
                write!(
                    f,
                    "not starting: {FILE_NAME} has no {key}. Add it, or delete {FILE_NAME} to take the development defaults."
                )
            }
            ConfigError::Invalid { key, why } => {
                write!(
                    f,
                    "not starting: {FILE_NAME}'s {key} is set to something unusable — {why}"
                )
            }
            ConfigError::Io(why) => write!(f, "not starting: {why}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    /// Reads `config/iz.toml` from the current directory, writing it with
    /// the development defaults first if it is not there.
    pub fn load() -> Result<Config, ConfigError> {
        Config::load_from(Path::new("."))
    }

    /// The same reading, against any directory — which is how it is tested
    /// without a test being able to disturb another test's working
    /// directory, or another test's `config/iz.toml`.
    pub fn load_from(dir: &Path) -> Result<Config, ConfigError> {
        let path = dir.join(FILE_NAME);
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let config = Config::parse(&text, dir, false)?;
                complete(&path, &text, dir);
                Ok(config)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|err| {
                        ConfigError::Io(format!("could not create {}: {err}", parent.display()))
                    })?;
                }
                std::fs::write(&path, DEVELOPMENT_DEFAULTS).map_err(|err| {
                    ConfigError::Io(format!("could not write {}: {err}", path.display()))
                })?;
                // The file will hold a live client secret once the admin
                // fills it in; close it now, before it does.
                let _ = crate::store::secret::restrict(&path);
                println!(
                    "iz    wrote {FILE_NAME} with development defaults — fill in [oidc] for a real deployment"
                );
                Config::parse(DEVELOPMENT_DEFAULTS, dir, true)
            }
            Err(err) => Err(ConfigError::Io(format!(
                "could not read {}: {err}",
                path.display()
            ))),
        }
    }

    /// The reading itself, against a string — which is how it is tested
    /// without a test being able to disturb another test's filesystem.
    pub fn parse(text: &str, dir: &Path, defaulted: bool) -> Result<Config, ConfigError> {
        let toml: Toml = toml::from_str(text).map_err(|err| ConfigError::Unparseable {
            why: err.to_string(),
        })?;

        let value = |raw: Option<String>| raw.filter(|value| !value.trim().is_empty());

        let database = value(toml.database).ok_or(ConfigError::Missing("database"))?;
        let database = absolute(dir, Path::new(&database));

        let storage = match value(toml.storage) {
            Some(raw) => absolute(dir, Path::new(&raw)),
            None => default_storage(&database),
        };

        let listen = value(toml.listen).unwrap_or_else(|| DEFAULT_LISTEN.to_string());
        let listen: SocketAddr = listen.parse().map_err(|err| ConfigError::Invalid {
            key: "listen",
            why: format!("{listen:?} is not a host:port address — {err}"),
        })?;

        // Zero would mean a stream that ends the moment it opens, which is a
        // reconnect loop rather than a live feed; the key is refused rather
        // than quietly corrected, because a deployment that meant to say
        // "never expire" should find out here and not at three in the morning.
        let live_seconds = toml.live_seconds.unwrap_or(DEFAULT_LIVE_SECONDS);
        if live_seconds == 0 {
            return Err(ConfigError::Invalid {
                key: "live_seconds",
                why: "0 would close every live connection as soon as it opened".to_string(),
            });
        }

        let oidc_toml = toml.oidc.ok_or(ConfigError::Missing("[oidc]"))?;
        let issuer = value(oidc_toml.issuer).ok_or(ConfigError::Missing("oidc.issuer"))?;
        let client_id = value(oidc_toml.client_id).ok_or(ConfigError::Missing("oidc.client_id"))?;
        let client_secret =
            value(oidc_toml.client_secret).ok_or(ConfigError::Missing("oidc.client_secret"))?;
        // Derived from the address bound, on every boot: a deployment behind
        // a proxy sets this key, and everyone else follows `listen` for free.
        let redirect_uri = value(oidc_toml.redirect_uri)
            .unwrap_or_else(|| format!("{}/auth/callback", listen_url_of(&listen)));

        let mut ignored: Vec<String> = toml.other.into_keys().collect();
        ignored.extend(oidc_toml.other.into_keys().map(|key| format!("oidc.{key}")));

        Ok(Config {
            database,
            storage,
            listen,
            live_seconds,
            oidc: OidcConfig {
                issuer,
                client_id,
                client_secret,
                redirect_uri,
            },
            ignored,
            defaulted,
        })
    }

    /// The address a mailed link points at until an admin sets one in
    /// Settings: the address the server binds, as a URL.
    ///
    /// A bind that names no interface — `0.0.0.0`, `::` — answers everywhere
    /// and is reachable at none of it by name, so the loopback stands in: a
    /// link somebody on the box can click beats a link nobody can. Whoever
    /// puts İz behind a proxy sets the real address in Settings, which is
    /// the only thing this defers to.
    pub fn listen_url(&self) -> String {
        listen_url_of(&self.listen)
    }

    /// The lines to print once at startup. Nothing secret is among them.
    pub fn report(&self) -> Vec<String> {
        let mut lines = vec![
            format!("database  {}", self.database.display()),
            format!("storage   {}", self.storage.display()),
            format!("mail url  {} until an admin sets one", self.listen_url()),
            format!("oidc      {}", self.oidc.issuer),
        ];
        lines.push("mail      the sender is in Settings, not here".to_string());
        if !self.ignored.is_empty() {
            lines.push(format!(
                "ignored   {FILE_NAME} sets {}, which nothing reads",
                self.ignored.join(", ")
            ));
        }
        if self.defaulted {
            lines.push(format!(
                "dev       {FILE_NAME} did not exist, development defaults written and taken"
            ));
        }
        lines
    }
}

/// The address bound, as a URL — the shape `redirect_uri` defaults from.
fn listen_url_of(listen: &SocketAddr) -> String {
    let ip = listen.ip();
    let host = if ip.is_unspecified() {
        "127.0.0.1".to_string()
    } else if listen.is_ipv6() {
        format!("[{ip}]")
    } else {
        ip.to_string()
    };
    match listen.port() {
        80 => format!("http://{host}"),
        port => format!("http://{host}:{port}"),
    }
}

/// Appends the keys the file does not mention, each with its own comment and
/// the default already in effect. The file is how a reader learns what can be
/// changed, so a key that is silently defaulted is a key nobody discovers.
///
/// Only ever adds, never rewrites: whatever else the file says — comments,
/// ordering, a value somebody chose — is theirs. A file that cannot be
/// written is not a reason not to start; the defaults it is missing are the
/// ones already in force, so the run is correct either way and the note says
/// what could not be done.
fn complete(path: &Path, text: &str, base: &Path) {
    let mut missing: Vec<(&str, String)> = OPTIONAL_KEYS
        .iter()
        .filter(|(key, _)| !mentions(text, key))
        .map(|(key, block)| (*key, (*block).to_string()))
        .collect();
    // `storage` has no fixed default to print: a file silent about it derives
    // the directory from where `database` points, so the completed line says
    // the value this boot actually resolved rather than a placeholder.
    if !mentions(text, "storage") {
        // The same base `parse` resolves against, or the completed line would
        // point somewhere this boot never looks (config/ vs the directory
        // the app runs from).
        let database: Option<String> = toml::from_str::<Toml>(text)
            .ok()
            .and_then(|t| t.database)
            .filter(|value| !value.trim().is_empty());
        let storage = database
            .as_deref()
            .map(|db| default_storage(&absolute(base, Path::new(db))))
            .unwrap_or_else(|| PathBuf::from("storage"));
        missing.push((
            "storage",
            format!(
                "# Where attachments live as files, created on boot.\n\
                 storage = {:?}\n",
                storage.display().to_string()
            ),
        ));
    }
    if missing.is_empty() {
        return;
    }
    // Top-level keys cannot live past a `[table]` header — TOML reads them
    // as belonging to the table — so the completed block lands before the
    // first header, never inside `[oidc]`.
    let block = missing
        .iter()
        .map(|(_, block)| block.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let mut completed = text.to_string();
    match completed
        .lines()
        .position(|line| line.trim_start().starts_with('['))
    {
        Some(at) => {
            let mut lines: Vec<&str> = completed.lines().collect();
            lines.insert(at, block.as_str());
            completed = lines.join("\n");
            if text.ends_with('\n') || completed.is_empty() {
                completed.push('\n');
            }
        }
        None => {
            if !completed.is_empty() && !completed.ends_with('\n') {
                completed.push('\n');
            }
            completed.push('\n');
            completed.push_str(&block);
        }
    }
    match std::fs::write(path, completed) {
        Ok(()) => println!(
            "iz    added {} to {FILE_NAME}, at the default already in use",
            missing
                .iter()
                .map(|(key, _)| *key)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Err(err) => println!("iz    could not complete {FILE_NAME}: {err}"),
    }
}

/// Whether the file sets this key — a line whose first word it is. A key
/// named inside a comment or a value does not count.
fn mentions(text: &str, key: &str) -> bool {
    text.lines().any(|line| {
        line.trim_start()
            .strip_prefix(key)
            .is_some_and(|rest| rest.trim_start().starts_with('='))
    })
}

/// Where binary files live when the file is silent about it: beside the
/// database file. The two are one backup unit — a backup that takes the
/// database but not the files beside it restores a board whose attachments
/// and photos are gone — so the default keeps them siblings.
fn default_storage(database: &Path) -> PathBuf {
    database
        .parent()
        .map(|parent| parent.join("storage"))
        .unwrap_or_else(|| PathBuf::from("storage"))
}

/// An absolute path for a file that may not exist yet: the directory is
/// resolved against `base` (the directory `config/iz.toml` was read
/// from), the file name is kept as written.
fn absolute(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    let base = base
        .canonicalize()
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let joined = base.join(path);
    match (joined.parent(), joined.file_name()) {
        (Some(parent), Some(name)) => match parent.canonicalize() {
            Ok(parent) => parent.join(name),
            Err(_) => joined.clone(),
        },
        _ => joined,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory, cleaned up when the test ends, so `load_from` can
    /// be exercised without touching the process's own working directory.
    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("iz-config-test-{}", unique_suffix()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Good enough uniqueness for a scratch directory name; no need for a
    /// real ULID dependency just for this.
    fn unique_suffix() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            ^ (std::process::id() as u128) << 64
    }

    /// The `[oidc]` table every other test carries: those tests are about
    /// other keys, and without it the boot stops on the missing provider.
    const OIDC: &str = "[oidc]\nissuer = \"https://id.example.com\"\nclient_id = \"iz\"\nclient_secret = \"s3cret\"\n";

    #[test]
    fn an_absent_file_is_written_with_development_defaults_and_then_refuses_oidc() {
        let dir = scratch();
        // The fresh file has blank [oidc] keys, so the first boot writes and
        // then stops on the first of them.
        let err = Config::load_from(&dir).unwrap_err();
        assert_eq!(err, ConfigError::Missing("oidc.issuer"));

        let written = std::fs::read_to_string(dir.join(FILE_NAME)).unwrap();
        assert_eq!(written, DEVELOPMENT_DEFAULTS);

        // The file it wrote still has blanks, so a second load stops the
        // same way rather than starting half-authenticated.
        assert_eq!(
            Config::load_from(&dir).unwrap_err(),
            ConfigError::Missing("oidc.issuer")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
    /// Every key the file can carry is written into a file that was made
    /// without it, so what can be changed is learnt by reading the file
    /// rather than by reading the source.
    #[test]
    fn a_file_missing_a_key_is_completed_with_it() {
        let dir = scratch();
        std::fs::create_dir_all(dir.join("config")).unwrap();
        std::fs::write(
            dir.join(FILE_NAME),
            &format!("# mine\ndatabase = \"state/iz.db\"\n{OIDC}"),
        )
        .unwrap();

        let config = Config::load_from(&dir).unwrap();
        assert_eq!(config.listen, "127.0.0.1:7654".parse().unwrap());

        let written = std::fs::read_to_string(dir.join(FILE_NAME)).unwrap();
        assert!(written.contains("listen = \"127.0.0.1:7654\""), "{written}");
        // `storage` has no literal default: the completed line is where the
        // file's own `database` put it — beside `state/iz.db` here.
        assert!(written.contains("storage = "), "{written}");
        // What was there is untouched, comment and all.
        assert!(
            written.starts_with("# mine\ndatabase = \"state/iz.db\"\n"),
            "{written}"
        );

        // Completing is not rewriting: a second load leaves the file alone,
        // and reads the completed key as the default already in use.
        let again = Config::load_from(&dir).unwrap();
        assert_eq!(again.storage, config.storage);
        assert_eq!(
            std::fs::read_to_string(dir.join(FILE_NAME)).unwrap(),
            written
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A key that is already set is not touched, whatever its value.
    #[test]
    fn completion_leaves_a_key_that_is_already_there() {
        let dir = scratch();
        std::fs::create_dir_all(dir.join("config")).unwrap();
        let mine = &format!(
            "database = \"iz.db\"\nstorage = \"files\"\nlisten = \"0.0.0.0:8080\"\nlive_seconds = 30\n{OIDC}"
        );
        std::fs::write(dir.join(FILE_NAME), mine).unwrap();
        let config = Config::load_from(&dir).unwrap();
        assert_eq!(config.listen, "0.0.0.0:8080".parse().unwrap());
        assert_eq!(config.live_seconds, 30);
        assert!(config.storage.ends_with("files"), "{:?}", config.storage);
        assert_eq!(std::fs::read_to_string(dir.join(FILE_NAME)).unwrap(), mine.as_str());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Binary lives where the file says, or — when it says nothing — beside
    /// the database, so a backup that takes the database takes the files.
    #[test]
    fn a_file_without_storage_puts_it_beside_the_database() {
        let config = Config::parse(
            &format!("database = \"/srv/iz/iz.db\"\n{OIDC}"),
            Path::new("."),
            false,
        )
        .unwrap();
        assert_eq!(config.storage, PathBuf::from("/srv/iz/storage"));
    }

    /// An explicit storage is taken absolute, the way the database is.
    #[test]
    fn a_relative_storage_is_reported_absolute() {
        let dir = scratch();
        let config = Config::parse(
            &format!("database = \"iz.db\"\nstorage = \"state/files\"\n{OIDC}"),
            &dir,
            false,
        )
        .unwrap();
        assert!(config.storage.is_absolute(), "{:?}", config.storage);
        assert!(
            config.storage.ends_with("state/files"),
            "{:?}",
            config.storage
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A live connection that expires instantly is a reconnect loop, so the
    /// key is refused rather than silently corrected.
    #[test]
    fn a_zero_live_window_stops_the_boot_naming_the_key() {
        let problem = Config::parse(
            "database = \"iz.db\"\nlive_seconds = 0\n",
            Path::new("."),
            false,
        )
        .unwrap_err();
        assert!(
            matches!(
                problem,
                ConfigError::Invalid {
                    key: "live_seconds",
                    ..
                }
            ),
            "{problem:?}"
        );
    }

    /// A file that never mentions the key still boots, on the default.
    #[test]
    fn a_file_without_a_live_window_takes_the_default() {
        let config = Config::parse(&format!("database = \"iz.db\"\n{OIDC}"), Path::new("."), false)
            .unwrap();
        assert_eq!(config.live_seconds, 300);
    }

    #[test]
    fn an_empty_string_is_not_a_value() {
        let problem = Config::parse("database = \"   \"\n", Path::new("."), false).unwrap_err();
        assert_eq!(problem, ConfigError::Missing("database"));
    }

    #[test]
    fn a_missing_key_names_itself_rather_than_guessing() {
        let problem =
            Config::parse("listen = \"127.0.0.1:7654\"\n", Path::new("."), false).unwrap_err();
        assert_eq!(problem, ConfigError::Missing("database"));
        let said = problem.to_string();
        assert!(said.contains("database"), "{said}");
        assert!(said.contains("iz.toml"), "{said}");
    }

    #[test]
    fn unparseable_toml_names_the_file_not_a_stack_trace() {
        let problem =
            Config::parse("this is not toml at all {{{", Path::new("."), false).unwrap_err();
        assert!(
            matches!(problem, ConfigError::Unparseable { .. }),
            "{problem:?}"
        );
        assert!(problem.to_string().contains("iz.toml"));
    }

    #[test]
    fn a_relative_database_is_reported_absolute() {
        let dir = scratch();
        let config = Config::parse(&format!("database = \"state/iz.db\"\n{OIDC}"), &dir, false)
            .unwrap();
        assert!(config.database.is_absolute(), "{:?}", config.database);
        assert!(config.database.ends_with("iz.db"));
        let report = config.report().join("\n");
        assert!(
            report.contains(&config.database.display().to_string()),
            "{report}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file that does not mention `listen` still loads, and binds at the
    /// default it is about to be completed with.
    #[test]
    fn a_file_without_listen_takes_the_default() {
        let config =
            Config::parse(&format!("database = \"/srv/iz.db\"\n{OIDC}"), Path::new("."), false)
                .unwrap();
        assert_eq!(config.listen, "127.0.0.1:7654".parse().unwrap());
    }

    #[test]
    fn an_unparseable_listen_stops_the_boot_naming_the_key() {
        let problem = Config::parse(
            "database = \"/srv/iz.db\"\nlisten = \"not an address\"\n",
            Path::new("."),
            false,
        )
        .unwrap_err();
        assert!(
            matches!(problem, ConfigError::Invalid { key: "listen", .. }),
            "{problem:?}"
        );
    }

    /// The address mail falls back to is the address the server answers on —
    /// there is no second key to keep in step with the first.
    #[test]
    fn the_mail_fallback_is_the_address_the_server_binds() {
        let url = |listen: &str| {
            Config::parse(
                &format!("database = \"/srv/iz.db\"\nlisten = \"{listen}\"\n{OIDC}"),
                Path::new("."),
                false,
            )
            .unwrap()
            .listen_url()
        };
        assert_eq!(url("127.0.0.1:7654"), "http://127.0.0.1:7654");
        assert_eq!(url("192.168.1.20:8080"), "http://192.168.1.20:8080");
        // Port 80 is the one a URL does not say.
        assert_eq!(url("10.0.0.4:80"), "http://10.0.0.4");
        // A bind that names no interface is reachable by no name, so the
        // loopback stands in: a link somebody on the box can click beats a
        // link nobody can.
        assert_eq!(url("0.0.0.0:7654"), "http://127.0.0.1:7654");
        assert_eq!(url("[::]:7654"), "http://127.0.0.1:7654");
        assert_eq!(url("[::1]:7654"), "http://[::1]:7654");
    }

    /// The sender used to be environment variables read directly, and the
    /// base URL used to be a key of its own. Both are gone, so a file that
    /// still carries them must do nothing at all — not half-configure a
    /// sender, not stop the boot, and above all not quietly send through an
    /// account the Settings screen does not show. The report says they were
    /// seen, so a typo is visible too.
    #[test]
    fn keys_nothing_reads_are_named_rather_than_obeyed() {
        let config = Config::parse(
            &format!(
                "database = \"/srv/iz.db\"\nbase_url = \"https://iz.sh\"\n\
                 smtp_password = \"hunter2-and-then-some\"\n{OIDC}"
            ),
            Path::new("."),
            false,
        )
        .expect("a key the file no longer honours is not an error");

        assert_eq!(config.ignored, vec!["base_url", "smtp_password"]);
        let report = config.report().join("\n");
        assert!(
            !report.contains("hunter2-and-then-some"),
            "the report read a key it no longer honours: {report}"
        );
        assert!(report.contains("base_url"), "{report}");
        assert!(
            report.contains("the sender is in Settings"),
            "the report should say where the sender lives: {report}"
        );
    }

    #[test]
    fn a_missing_oidc_table_names_itself() {
        let problem = Config::parse("database = \"iz.db\"\n", Path::new("."), false).unwrap_err();
        assert_eq!(problem, ConfigError::Missing("[oidc]"));
    }

    #[test]
    fn an_oidc_section_with_blanks_stops_on_the_first_blank() {
        let problem = Config::parse(
            "database = \"iz.db\"\n[oidc]\nissuer = \"\"\nclient_id = \"iz\"\nclient_secret = \"s3cret\"\n",
            Path::new("."),
            false,
        )
        .unwrap_err();
        assert_eq!(problem, ConfigError::Missing("oidc.issuer"));
    }

    #[test]
    fn redirect_uri_defaults_from_listen() {
        let config = Config::parse(
            &format!("database = \"iz.db\"\nlisten = \"0.0.0.0:9000\"\n{OIDC}"),
            Path::new("."),
            false,
        )
        .unwrap();
        // A bind that names no interface is reachable by no name, so the
        // loopback stands in here too.
        assert_eq!(
            config.oidc.redirect_uri,
            "http://127.0.0.1:9000/auth/callback"
        );
        assert_eq!(config.oidc.issuer, "https://id.example.com");
    }

    #[test]
    fn an_explicit_redirect_uri_wins() {
        let config = Config::parse(
            "database = \"iz.db\"\n[oidc]\nissuer = \"https://id.example.com\"\nclient_id = \"iz\"\n\
             client_secret = \"s3cret\"\nredirect_uri = \"https://iz.example.com/auth/callback\"\n",
            Path::new("."),
            false,
        )
        .unwrap();
        assert_eq!(
            config.oidc.redirect_uri,
            "https://iz.example.com/auth/callback"
        );
    }

    #[test]
    fn a_key_inside_oidc_nothing_reads_is_named_with_its_table() {
        let config = Config::parse(
            &format!("database = \"iz.db\"\n{OIDC}oidc_typo = 1\n"),
            Path::new("."),
            false,
        )
        .unwrap();
        assert!(config.ignored.contains(&"oidc.oidc_typo".to_string()));
        assert!(config.report().iter().any(|line| line.contains("oidc.oidc_typo")));
    }

    #[test]
    fn completed_keys_land_before_the_oidc_table() {
        let dir = scratch();
        std::fs::create_dir_all(dir.join("config")).unwrap();
        std::fs::write(
            dir.join(FILE_NAME),
            &format!("database = \"iz.db\"\n{OIDC}"),
        )
        .unwrap();
        Config::load_from(&dir).unwrap();
        let completed = std::fs::read_to_string(dir.join(FILE_NAME)).unwrap();
        // Completed top-level keys land before the [oidc] header, never
        // inside it.
        let oidc_at = completed.find("[oidc]").unwrap();
        let listen_at = completed.find("listen = ").unwrap();
        assert!(listen_at < oidc_at, "{completed}");
        let reparsed: toml::Value = toml::from_str(&completed).unwrap();
        assert!(reparsed.get("listen").is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
