//! Everything Izlek reads from `config/izlek.toml`, in one place.
//!
//! Nothing here has a silent default once the file exists. A key that is
//! missing, empty or unusable stops the boot and says which key and which
//! file, because the alternative is worse than not starting: a wrong
//! `database` does not mean "no data", it means a second Izlek quietly
//! writing a different file while everyone believes they are looking at the
//! same board — and Turso is single-writer, so the two are not even
//! reconcilable afterwards. A wrong `base_url` mails people a sign-in link
//! pointing at a host that is not us.
//!
//! Development still needs to be one command, so the *absence* of
//! `config/izlek.toml` is the opt-in that takes the development defaults: the
//! app writes the file itself, with those defaults in it and comments saying
//! what each key does, and starts. That is opt-in on purpose too — it only
//! ever happens once, because the second boot finds the file it wrote the
//! first time and reads it like any other. A real deployment is handed the
//! same file and edits it; it never writes itself over a deployment's
//! choices, because it only writes when the file is not there at all.
//!
//! Whatever is finally resolved is printed once at startup — the database
//! path absolute, the base URL as it will appear in mail — so "which file
//! are we on" is answered by the log rather than by someone's memory.
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
const FILE_NAME: &str = "config/izlek.toml";

/// What a freshly written `config/izlek.toml` says, comments included. Development
/// defaults, so that a plain `izlek` in an empty directory is one command.
const DEVELOPMENT_DEFAULTS: &str = r#"# Where the one database file lives. One process holds it.
database = "izlek.db"
# The origin sign-in links in mail point at.
base_url = "http://127.0.0.1:3000"
# The address the server listens on. Environment variables are ignored —
# this is the only thing that decides where Izlek binds.
listen = "127.0.0.1:3000"
"#;

/// The default `listen` when the file is silent about it — an existing
/// deployment's `config/izlek.toml` from before this key existed still
/// loads, and still listens where it always did.
const DEFAULT_LISTEN: &str = "127.0.0.1:3000";

/// The shape of `config/izlek.toml`, before the values are checked.
#[derive(Deserialize)]
struct Toml {
    database: Option<String>,
    base_url: Option<String>,
    listen: Option<String>,
}

/// What the process needs to know before it opens a socket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// The database file, absolute. One process holds it.
    pub database: PathBuf,
    /// The origin links in mail point at, with no trailing slash.
    pub base_url: String,
    /// The address the server binds. The only source for this — `HOST` and
    /// `PORT` environment variables are never read.
    pub listen: SocketAddr,
    /// Whether `config/izlek.toml` did not exist and was just written with the
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
    /// Reads `config/izlek.toml` from the current directory, writing it with
    /// the development defaults first if it is not there.
    pub fn load() -> Result<Config, ConfigError> {
        Config::load_from(Path::new("."))
    }

    /// The same reading, against any directory — which is how it is tested
    /// without a test being able to disturb another test's working
    /// directory, or another test's `config/izlek.toml`.
    pub fn load_from(dir: &Path) -> Result<Config, ConfigError> {
        let path = dir.join(FILE_NAME);
        match std::fs::read_to_string(&path) {
            Ok(text) => Config::parse(&text, dir, false),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|err| {
                        ConfigError::Io(format!("could not create {}: {err}", parent.display()))
                    })?;
                }
                std::fs::write(&path, DEVELOPMENT_DEFAULTS).map_err(|err| {
                    ConfigError::Io(format!("could not write {}: {err}", path.display()))
                })?;
                println!(
                    "izlek    wrote {FILE_NAME} with development defaults — edit it for a real deployment"
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
        let toml: Toml =
            toml::from_str(text).map_err(|err| ConfigError::Unparseable { why: err.to_string() })?;

        let value = |raw: Option<String>| raw.filter(|value| !value.trim().is_empty());

        let database = value(toml.database).ok_or(ConfigError::Missing("database"))?;
        let base_url = value(toml.base_url).ok_or(ConfigError::Missing("base_url"))?;

        if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
            return Err(ConfigError::Invalid {
                key: "base_url",
                why: format!("{base_url:?} is not an http:// or https:// origin"),
            });
        }
        let base_url = base_url.trim_end_matches('/').to_string();

        let listen = value(toml.listen).unwrap_or_else(|| DEFAULT_LISTEN.to_string());
        let listen: SocketAddr = listen.parse().map_err(|err| ConfigError::Invalid {
            key: "listen",
            why: format!("{listen:?} is not a host:port address — {err}"),
        })?;

        Ok(Config {
            database: absolute(dir, Path::new(&database)),
            base_url,
            listen,
            defaulted,
        })
    }

    /// The lines to print once at startup. Nothing secret is among them.
    pub fn report(&self) -> Vec<String> {
        let mut lines = vec![
            format!("database  {}", self.database.display()),
            format!("base url  {}", self.base_url),
            format!("listen    {}", self.listen),
        ];
        lines.push("mail      the sender is in Settings, not here".to_string());
        if self.defaulted {
            lines.push(format!(
                "dev       {FILE_NAME} did not exist, development defaults written and taken"
            ));
        }
        lines
    }
}

/// An absolute path for a file that may not exist yet: the directory is
/// resolved against `base` (the directory `config/izlek.toml` was read
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
        let dir = std::env::temp_dir().join(format!("izlek-config-test-{}", unique_suffix()));
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

    #[test]
    fn an_absent_file_is_written_with_development_defaults_and_taken() {
        let dir = scratch();
        let config = Config::load_from(&dir).unwrap();
        assert!(config.defaulted);
        assert!(config.database.is_absolute(), "{:?}", config.database);
        assert!(config.database.ends_with("izlek.db"));
        assert_eq!(config.base_url, "http://127.0.0.1:3000");
        assert_eq!(config.listen, "127.0.0.1:3000".parse().unwrap());

        let written = std::fs::read_to_string(dir.join(FILE_NAME)).unwrap();
        assert_eq!(written, DEVELOPMENT_DEFAULTS);

        // A second load reads the file it just wrote, identically.
        let again = Config::load_from(&dir).unwrap();
        assert!(!again.defaulted);
        assert_eq!(again, Config { defaulted: false, ..config });
    }

    #[test]
    fn an_empty_string_is_not_a_value() {
        let problem = Config::parse(
            "database = \"   \"\nbase_url = \"https://izlek.sh\"\n",
            Path::new("."),
            false,
        )
        .unwrap_err();
        assert_eq!(problem, ConfigError::Missing("database"));
    }

    #[test]
    fn a_missing_key_names_itself_rather_than_guessing() {
        let problem = Config::parse("base_url = \"https://izlek.sh\"\n", Path::new("."), false)
            .unwrap_err();
        assert_eq!(problem, ConfigError::Missing("database"));
        let said = problem.to_string();
        assert!(said.contains("database"), "{said}");
        assert!(said.contains("izlek.toml"), "{said}");
    }

    #[test]
    fn unparseable_toml_names_the_file_not_a_stack_trace() {
        let problem = Config::parse("this is not toml at all {{{", Path::new("."), false)
            .unwrap_err();
        assert!(
            matches!(problem, ConfigError::Unparseable { .. }),
            "{problem:?}"
        );
        assert!(problem.to_string().contains("izlek.toml"));
    }

    #[test]
    fn a_relative_database_is_reported_absolute() {
        let dir = scratch();
        let config = Config::parse(
            "database = \"state/izlek.db\"\nbase_url = \"https://izlek.sh\"\n",
            &dir,
            false,
        )
        .unwrap();
        assert!(config.database.is_absolute(), "{:?}", config.database);
        assert!(config.database.ends_with("izlek.db"));
        let report = config.report().join("\n");
        assert!(
            report.contains(&config.database.display().to_string()),
            "{report}"
        );
    }

    #[test]
    fn a_base_url_that_is_not_an_origin_stops_the_boot() {
        let problem = Config::parse(
            "database = \"/srv/izlek.db\"\nbase_url = \"izlek.sh\"\n",
            Path::new("."),
            false,
        )
        .unwrap_err();
        assert!(
            matches!(
                problem,
                ConfigError::Invalid {
                    key: "base_url",
                    ..
                }
            ),
            "{problem:?}"
        );
    }

    /// A `config/izlek.toml` written before `listen` existed must still load,
    /// and must still bind where it always did.
    #[test]
    fn a_file_without_listen_falls_back_to_the_old_default() {
        let config = Config::parse(
            "database = \"/srv/izlek.db\"\nbase_url = \"https://izlek.sh\"\n",
            Path::new("."),
            false,
        )
        .unwrap();
        assert_eq!(config.listen, "127.0.0.1:3000".parse().unwrap());
    }

    #[test]
    fn an_unparseable_listen_stops_the_boot_naming_the_key() {
        let problem = Config::parse(
            "database = \"/srv/izlek.db\"\nbase_url = \"https://izlek.sh\"\nlisten = \"not an address\"\n",
            Path::new("."),
            false,
        )
        .unwrap_err();
        assert!(
            matches!(problem, ConfigError::Invalid { key: "listen", .. }),
            "{problem:?}"
        );
    }

    #[test]
    fn the_base_url_keeps_no_trailing_slash_so_links_are_built_once() {
        let config = Config::parse(
            "database = \"/srv/izlek.db\"\nbase_url = \"https://izlek.sh/\"\n",
            Path::new("."),
            false,
        )
        .unwrap();
        assert_eq!(config.base_url, "https://izlek.sh");
    }

    /// The sender used to be environment variables read directly. It is
    /// workspace settings now, so unrelated keys left in the file must do
    /// nothing at all — not half-configure a sender, not stop the boot, and
    /// above all not quietly send through an account the Settings screen
    /// does not show.
    #[test]
    fn unrelated_keys_are_ignored_entirely() {
        let config = Config::parse(
            "database = \"/srv/izlek.db\"\nbase_url = \"https://izlek.sh\"\nsmtp_password = \"hunter2-and-then-some\"\n",
            Path::new("."),
            false,
        )
        .expect("an unrelated key in the file is not an error");

        let report = config.report().join("\n");
        assert!(
            !report.contains("hunter2-and-then-some"),
            "the report read a key it no longer honours: {report}"
        );
        assert!(
            report.contains("the sender is in Settings"),
            "the report should say where the sender lives: {report}"
        );
    }
}
