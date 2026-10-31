//! Server configuration.
//!
//! Precedence: real process environment variables win over `.env` values.
//! `.env` is loaded from the current working directory only (the directory the
//! MCP client spawns the server in). `dotenvy` only fills variables that are
//! not already present, so an explicit MCP `env` block is never overridden.

use std::env;
use std::path::PathBuf;
use std::time::Duration;

use url::Url;

use crate::error::{Error, Result};
use crate::log::Level;

/// Default request timeout (30 seconds).
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MIN_TIMEOUT_MS: u64 = 1_000;
const MAX_TIMEOUT_MS: u64 = 300_000;

/// Validated runtime configuration.
#[derive(Clone)]
pub struct Config {
    /// Redmine base URL. Always normalized to end with `/`.
    pub redmine_url: Url,
    /// Redmine API key (secret).
    pub api_key: String,
    /// Project used by `create_issue` when `project_id` is omitted.
    pub default_project_id: Option<u64>,
    /// Total timeout for a single HTTP request.
    pub timeout: Duration,
    /// When true, write tools are hidden and rejected.
    pub read_only: bool,
    /// stderr verbosity.
    pub log_level: Level,
}

/// Non-fatal facts discovered while loading the configuration.
#[derive(Debug, Default)]
pub struct StartupInfo {
    /// `.env` file that was loaded, if any.
    pub dotenv_path: Option<PathBuf>,
    /// Problems worth reporting (for example, a malformed `.env` that did not
    /// prevent startup because the real environment provided the values).
    pub warnings: Vec<String>,
}

// The API key must never be printed, not even in debug output.
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("redmine_url", &self.redmine_url.as_str())
            .field("api_key", &"<redacted>")
            .field("default_project_id", &self.default_project_id)
            .field("timeout", &self.timeout)
            .field("read_only", &self.read_only)
            .field("log_level", &self.log_level)
            .finish()
    }
}

impl Config {
    /// Loads `.env` (optional) and validates the environment.
    pub fn load() -> Result<(Self, StartupInfo)> {
        let (dotenv_path, dotenv_warning) = load_dotenv();
        let mut info = StartupInfo {
            dotenv_path,
            warnings: Vec::new(),
        };

        match Self::from_lookup(|name| env::var(name).ok()) {
            Ok(config) => {
                info.warnings.extend(dotenv_warning);
                Ok((config, info))
            }
            Err(Error::Config(message)) => match dotenv_warning {
                Some(warning) => Err(Error::config(format!("{message}; additionally, {warning}"))),
                None => Err(Error::config(message)),
            },
            Err(other) => Err(other),
        }
    }

    /// Testable constructor. The lookup receives variable names and returns
    /// the raw value, if set.
    pub fn from_lookup<F>(lookup: F) -> Result<Self>
    where
        F: Fn(&str) -> Option<String>,
    {
        let raw_url = required(&lookup, "REDMINE_URL")?;
        let api_key = required(&lookup, "REDMINE_API_KEY")?;
        let redmine_url = normalize_url(&raw_url)?;

        let default_project_id = match get(&lookup, "REDMINE_DEFAULT_PROJECT_ID") {
            None => None,
            Some(value) => Some(parse_positive_u64("REDMINE_DEFAULT_PROJECT_ID", &value)?),
        };

        let timeout = match get(&lookup, "REDMINE_TIMEOUT_MS") {
            None => Duration::from_millis(DEFAULT_TIMEOUT_MS),
            Some(value) => {
                let millis: u64 = value.parse().map_err(|_| {
                    Error::config(format!(
                        "REDMINE_TIMEOUT_MS must be an integer number of milliseconds (got {value:?})"
                    ))
                })?;
                if !(MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS).contains(&millis) {
                    return Err(Error::config(format!(
                        "REDMINE_TIMEOUT_MS must be between {MIN_TIMEOUT_MS} and {MAX_TIMEOUT_MS} (got {millis})"
                    )));
                }
                Duration::from_millis(millis)
            }
        };

        let read_only = match get(&lookup, "REDMINE_READ_ONLY") {
            None => false,
            Some(value) => parse_bool("REDMINE_READ_ONLY", &value)?,
        };

        let log_level = match get(&lookup, "REDMINE_LOG") {
            None => Level::Error,
            Some(value) => Level::parse(&value).ok_or_else(|| {
                Error::config(format!(
                    "REDMINE_LOG must be one of off, error, warn, info or debug (got {value:?})"
                ))
            })?,
        };

        Ok(Self {
            redmine_url,
            api_key,
            default_project_id,
            timeout,
            read_only,
            log_level,
        })
    }

    /// Normalized base URL as a string (always ends with `/`).
    pub fn base_url(&self) -> &str {
        self.redmine_url.as_str()
    }
}

fn get<F>(lookup: &F, name: &str) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    lookup(name)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn required<F>(lookup: &F, name: &str) -> Result<String>
where
    F: Fn(&str) -> Option<String>,
{
    get(lookup, name).ok_or_else(|| {
        let hint = match name {
            "REDMINE_URL" => " (for example https://redmine.example.com/)",
            "REDMINE_API_KEY" => " (create one in Redmine under My account > API access key)",
            _ => "",
        };
        Error::config(format!("{name} is required{hint}"))
    })
}

fn parse_positive_u64(name: &str, value: &str) -> Result<u64> {
    let parsed: u64 = value
        .parse()
        .map_err(|_| Error::config(format!("{name} must be a positive integer (got {value:?})")))?;
    if parsed == 0 {
        return Err(Error::config(format!("{name} must be greater than 0")));
    }
    Ok(parsed)
}

fn parse_bool(name: &str, value: &str) -> Result<bool> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(Error::config(format!(
            "{name} must be one of true/false, 1/0, yes/no or on/off (got {value:?})"
        ))),
    }
}

fn normalize_url(raw: &str) -> Result<Url> {
    let mut url = Url::parse(raw)
        .map_err(|err| Error::config(format!("REDMINE_URL is not a valid URL: {err}")))?;

    match url.scheme() {
        "http" | "https" => {}
        other => {
            return Err(Error::config(format!(
                "REDMINE_URL must use http or https (got scheme {other:?})"
            )));
        }
    }

    if url.host_str().is_none() {
        return Err(Error::config("REDMINE_URL must include a host"));
    }

    if !url.username().is_empty() || url.password().is_some() {
        return Err(Error::config(
            "REDMINE_URL must not contain credentials; use REDMINE_API_KEY instead",
        ));
    }

    if url.query().is_some() || url.fragment().is_some() {
        return Err(Error::config(
            "REDMINE_URL must not contain a query string or fragment",
        ));
    }

    if !url.path().ends_with('/') {
        // path_segments_mut appends a trailing slash without re-encoding the
        // existing path (set_path would turn "%20" into "%2520").
        url.path_segments_mut()
            .map_err(|_| Error::config("REDMINE_URL must be a hierarchical http(s) URL"))?
            .push("");
    }

    Ok(url)
}

fn load_dotenv() -> (Option<PathBuf>, Option<String>) {
    let path = match env::current_dir() {
        Ok(dir) => dir.join(".env"),
        Err(_) => return (None, None),
    };
    if !path.is_file() {
        return (None, None);
    }
    match dotenvy::from_path(&path) {
        Ok(()) => (Some(path), None),
        Err(err) => (
            Some(path.clone()),
            Some(format!("failed to load {}: {err}", path.display())),
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn config(vars: &[(&str, &str)]) -> Result<Config> {
        let map: HashMap<String, String> = vars
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        Config::from_lookup(|name| map.get(name).cloned())
    }

    fn valid() -> Vec<(&'static str, &'static str)> {
        vec![
            ("REDMINE_URL", "https://redmine.example.com"),
            ("REDMINE_API_KEY", "0123456789abcdef0123456789abcdef"),
        ]
    }

    #[test]
    fn minimal_config_uses_defaults() {
        let cfg = config(&valid()).expect("valid");
        assert_eq!(cfg.base_url(), "https://redmine.example.com/");
        assert_eq!(cfg.timeout, Duration::from_millis(DEFAULT_TIMEOUT_MS));
        assert_eq!(cfg.default_project_id, None);
        assert!(!cfg.read_only);
        assert_eq!(cfg.log_level, Level::Error);
    }

    #[test]
    fn normalizes_base_path_without_trailing_slash() {
        let mut vars = valid();
        vars[0] = ("REDMINE_URL", "https://example.com/redmine");
        let cfg = config(&vars).expect("valid");
        assert_eq!(cfg.base_url(), "https://example.com/redmine/");
    }

    #[test]
    fn keeps_existing_trailing_slash() {
        let mut vars = valid();
        vars[0] = ("REDMINE_URL", "https://example.com/redmine/");
        let cfg = config(&vars).expect("valid");
        assert_eq!(cfg.base_url(), "https://example.com/redmine/");
    }

    #[test]
    fn preserves_percent_encoded_base_paths() {
        let mut vars = valid();
        vars[0] = ("REDMINE_URL", "https://example.com/redmine%20project");
        let cfg = config(&vars).expect("valid");
        assert_eq!(cfg.base_url(), "https://example.com/redmine%20project/");
    }

    #[test]
    fn trims_whitespace_around_values() {
        let cfg = config(&[
            ("REDMINE_URL", "  https://example.com  "),
            ("REDMINE_API_KEY", "  secret  "),
        ])
        .expect("valid");
        assert_eq!(cfg.api_key, "secret");
        assert_eq!(cfg.base_url(), "https://example.com/");
    }

    #[test]
    fn rejects_missing_values() {
        let err = config(&[]).expect_err("must fail");
        assert!(err.to_string().contains("REDMINE_URL"));
    }

    #[test]
    fn treats_blank_values_as_missing() {
        let err = config(&[("REDMINE_URL", "   "), ("REDMINE_API_KEY", "secret")])
            .expect_err("must fail");
        assert!(err.to_string().contains("REDMINE_URL"));
    }

    #[test]
    fn rejects_invalid_urls() {
        for url in [
            "not a url",
            "ftp://example.com",
            "https://user:pass@example.com",
            "https://example.com/?foo=bar",
            "https://example.com/#fragment",
        ] {
            let mut vars = valid();
            vars[0] = ("REDMINE_URL", url);
            assert!(config(&vars).is_err(), "URL should be rejected: {url}");
        }
    }

    #[test]
    fn parses_optional_values() {
        let mut vars = valid();
        vars.push(("REDMINE_DEFAULT_PROJECT_ID", "42"));
        vars.push(("REDMINE_TIMEOUT_MS", "5000"));
        vars.push(("REDMINE_READ_ONLY", "yes"));
        vars.push(("REDMINE_LOG", "debug"));
        let cfg = config(&vars).expect("valid");
        assert_eq!(cfg.default_project_id, Some(42));
        assert_eq!(cfg.timeout, Duration::from_millis(5_000));
        assert!(cfg.read_only);
        assert_eq!(cfg.log_level, Level::Debug);
    }

    #[test]
    fn rejects_invalid_optional_values() {
        let mut vars = valid();
        vars.push(("REDMINE_DEFAULT_PROJECT_ID", "0"));
        assert!(config(&vars).is_err());

        let mut vars = valid();
        vars.push(("REDMINE_TIMEOUT_MS", "10"));
        assert!(config(&vars).is_err());

        let mut vars = valid();
        vars.push(("REDMINE_READ_ONLY", "maybe"));
        assert!(config(&vars).is_err());

        let mut vars = valid();
        vars.push(("REDMINE_LOG", "loud"));
        assert!(config(&vars).is_err());
    }

    #[test]
    fn debug_output_redacts_api_key() {
        let cfg = config(&valid()).expect("valid");
        let debug = format!("{cfg:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("0123456789abcdef0123456789abcdef"));
    }
}
