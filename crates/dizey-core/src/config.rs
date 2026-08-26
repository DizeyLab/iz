//! Every environment variable Dizey reads, in one place.
//!
//! Nothing here has a silent default. A missing variable stops the boot and
//! says which name is missing, because the alternative is worse than not
//! starting: a wrong `DIZEY_DATABASE` does not mean "no data", it means a
//! second Dizey quietly writing a different file while everyone believes they
//! are looking at the same board — and Turso is single-writer, so the two are
//! not even reconcilable afterwards. A wrong `DIZEY_BASE_URL` mails people a
//! sign-in link pointing at a host that is not us.
//!
//! Development still needs to be one command, so `DIZEY_DEV=1` is the opt-in
//! that turns the defaults back on. It is opt-in on purpose: a deployment that
//! forgets it fails loudly, and a deployment that sets it did so deliberately.
//!
//! Whatever is finally resolved is printed once at startup — the database path
//! absolute, the base URL as it will appear in mail — so "which file are we
//! on" is answered by the log rather than by someone's memory.
//!
//! The sender is not here. Host, port, username, password and from-address are
//! workspace settings an admin writes on the Settings screen, so that changing
//! where mail goes out through does not need a shell on the box and a restart.

use std::fmt;
use std::path::{Path, PathBuf};

/// Every variable the app reads, in the order the report prints them.
pub const VARIABLES: &[&str] = &["DIZEY_DEV", "DIZEY_DATABASE", "DIZEY_BASE_URL"];

/// What the process needs to know before it opens a socket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// The database file, absolute. One process holds it.
    pub database: PathBuf,
    /// The origin links in mail point at, with no trailing slash.
    pub base_url: String,
    /// Whether the development defaults were taken.
    pub dev: bool,
}

/// Why the process is not starting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// Named variables are unset and have no default outside development.
    Missing(Vec<&'static str>),
    /// A variable is set to something the app cannot use.
    Invalid { variable: &'static str, why: String },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Missing(names) => {
                write!(f, "not starting: ")?;
                let them = if names.len() == 1 {
                    write!(f, "{} is not set", names[0])?;
                    "it"
                } else {
                    write!(f, "{} are not set", names.join(", "))?;
                    "them"
                };
                write!(
                    f,
                    ". Set {them}, or set DIZEY_DEV=1 to take the development defaults."
                )
            }
            ConfigError::Invalid { variable, why } => {
                write!(
                    f,
                    "not starting: {variable} is set to something unusable — {why}"
                )
            }
        }
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    /// Reads the environment. Every failure names the variable behind it.
    pub fn from_env() -> Result<Config, ConfigError> {
        Config::read(|name| std::env::var(name).ok())
    }

    /// The same reading, against any source — which is how it is tested
    /// without a test being able to disturb another test's environment.
    pub fn read(source: impl Fn(&str) -> Option<String>) -> Result<Config, ConfigError> {
        let value = |name: &str| {
            source(name).and_then(|raw| {
                let trimmed = raw.trim().to_string();
                (!trimmed.is_empty()).then_some(trimmed)
            })
        };
        let dev = matches!(value("DIZEY_DEV").as_deref(), Some("1" | "true" | "yes"));

        let mut missing = Vec::new();
        let database = value("DIZEY_DATABASE").or_else(|| dev.then(|| "dizey.db".to_string()));
        if database.is_none() {
            missing.push("DIZEY_DATABASE");
        }
        let base_url =
            value("DIZEY_BASE_URL").or_else(|| dev.then(|| "http://127.0.0.1:3000".to_string()));
        if base_url.is_none() {
            missing.push("DIZEY_BASE_URL");
        }
        if !missing.is_empty() {
            return Err(ConfigError::Missing(missing));
        }

        let base_url = base_url.expect("checked above");
        if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
            return Err(ConfigError::Invalid {
                variable: "DIZEY_BASE_URL",
                why: format!("{base_url:?} is not an http:// or https:// origin"),
            });
        }
        let base_url = base_url.trim_end_matches('/').to_string();

        Ok(Config {
            database: absolute(Path::new(&database.expect("checked above"))),
            base_url,
            dev,
        })
    }

    /// The lines to print once at startup. Nothing secret is among them.
    pub fn report(&self) -> Vec<String> {
        let mut lines = vec![
            format!("database  {}", self.database.display()),
            format!("base url  {}", self.base_url),
        ];
        lines.push("mail      the sender is in Settings, not here".to_string());
        if self.dev {
            lines.push("dev       DIZEY_DEV=1, development defaults taken".to_string());
        }
        lines
    }
}


/// An absolute path for a file that may not exist yet: the directory is
/// resolved, the file name is kept as written.
fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let joined = cwd.join(path);
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
    use std::collections::HashMap;

    /// An environment, without touching the process's own.
    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        move |name: &str| map.get(name).cloned()
    }

    #[test]
    fn an_empty_environment_names_what_is_missing_rather_than_guessing() {
        let problem = Config::read(env(&[])).unwrap_err();
        let ConfigError::Missing(names) = &problem else {
            panic!("expected the missing names, got {problem:?}");
        };
        assert_eq!(names, &["DIZEY_DATABASE", "DIZEY_BASE_URL"]);
        let said = problem.to_string();
        assert!(said.contains("DIZEY_DATABASE"), "{said}");
        assert!(said.contains("DIZEY_DEV=1"), "{said}");
    }

    #[test]
    fn a_blank_value_is_not_a_value() {
        let problem = Config::read(env(&[
            ("DIZEY_DATABASE", "   "),
            ("DIZEY_BASE_URL", "https://dizey.sh"),
        ]))
        .unwrap_err();
        assert_eq!(problem, ConfigError::Missing(vec!["DIZEY_DATABASE"]));
    }

    #[test]
    fn the_dev_flag_is_the_only_thing_that_brings_defaults_back() {
        let config = Config::read(env(&[("DIZEY_DEV", "1")])).unwrap();
        assert!(config.dev);
        assert!(config.database.is_absolute(), "{:?}", config.database);
        assert!(config.database.ends_with("dizey.db"));
        assert_eq!(config.base_url, "http://127.0.0.1:3000");
    }

    #[test]
    fn a_relative_database_is_reported_absolute() {
        let config = Config::read(env(&[
            ("DIZEY_DATABASE", "state/dizey.db"),
            ("DIZEY_BASE_URL", "https://dizey.sh"),
        ]))
        .unwrap();
        assert!(config.database.is_absolute(), "{:?}", config.database);
        assert!(config.database.ends_with("dizey.db"));
        let report = config.report().join("\n");
        assert!(
            report.contains(&config.database.display().to_string()),
            "{report}"
        );
    }

    #[test]
    fn a_base_url_that_is_not_an_origin_stops_the_boot() {
        let problem = Config::read(env(&[
            ("DIZEY_DATABASE", "/srv/dizey.db"),
            ("DIZEY_BASE_URL", "dizey.sh"),
        ]))
        .unwrap_err();
        assert!(
            matches!(
                problem,
                ConfigError::Invalid {
                    variable: "DIZEY_BASE_URL",
                    ..
                }
            ),
            "{problem:?}"
        );
    }

    #[test]
    fn the_base_url_keeps_no_trailing_slash_so_links_are_built_once() {
        let config = Config::read(env(&[
            ("DIZEY_DATABASE", "/srv/dizey.db"),
            ("DIZEY_BASE_URL", "https://dizey.sh/"),
        ]))
        .unwrap();
        assert_eq!(config.base_url, "https://dizey.sh");
    }

    /// The sender used to be five environment variables. It is workspace
    /// settings now, so a stale `DIZEY_SMTP_PASSWORD` left in a unit file or a
    /// shell must do nothing at all — not half-configure a sender, not stop the
    /// boot, and above all not quietly send through an account the Settings
    /// screen does not show.
    #[test]
    fn leftover_sender_variables_are_ignored_entirely() {
        let config = Config::read(env(&[
            ("DIZEY_DATABASE", "/srv/dizey.db"),
            ("DIZEY_BASE_URL", "https://dizey.sh"),
            ("DIZEY_SMTP_HOST", "smtp.example.net"),
            ("DIZEY_SMTP_PORT", "587"),
            ("DIZEY_SMTP_USERNAME", "dizey"),
            ("DIZEY_SMTP_PASSWORD", "hunter2-and-then-some"),
            ("DIZEY_MAIL_FROM", "board@dizey.sh"),
        ]))
        .expect("a stale sender in the environment is not an error");

        let report = config.report().join("\n");
        assert!(
            !report.contains("hunter2-and-then-some") && !report.contains("smtp.example.net"),
            "the report read a variable it no longer honours: {report}"
        );
        assert!(
            report.contains("the sender is in Settings"),
            "the report should say where the sender lives: {report}"
        );
    }

    #[test]
    fn every_variable_the_app_reads_is_named_in_one_list() {
        assert_eq!(VARIABLES.len(), 3, "a variable was added without a line here");
        assert!(VARIABLES.contains(&"DIZEY_DEV"));
        assert!(VARIABLES.contains(&"DIZEY_DATABASE"));
        assert!(VARIABLES.contains(&"DIZEY_BASE_URL"));
    }
}
