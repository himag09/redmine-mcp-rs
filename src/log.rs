//! Minimal stderr logger.
//!
//! MCP uses stdout exclusively for JSON-RPC traffic, so every diagnostic must
//! go to stderr. Logging is opt-in (`REDMINE_LOG`) and the default level only
//! reports fatal startup problems.
//!
//! Never log request headers, bodies or tool arguments: they can contain the
//! API key, issue text or note contents.

use std::sync::atomic::{AtomicU8, Ordering};

/// Log verbosity, ordered from least to most verbose.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Off = 0,
    Error = 1,
    Warn = 2,
    Info = 3,
    Debug = 4,
}

impl Level {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "quiet" => Some(Self::Off),
            "error" => Some(Self::Error),
            "warn" | "warning" => Some(Self::Warn),
            "info" => Some(Self::Info),
            "debug" | "trace" => Some(Self::Debug),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
        }
    }
}

static CURRENT_LEVEL: AtomicU8 = AtomicU8::new(Level::Error as u8);

/// Sets the global log level. Safe to call more than once.
pub fn init(level: Level) {
    CURRENT_LEVEL.store(level as u8, Ordering::Relaxed);
}

fn enabled(level: Level) -> bool {
    level != Level::Off && level as u8 <= CURRENT_LEVEL.load(Ordering::Relaxed)
}

/// Writes one log line to stderr. Public only so the macros can reach it.
pub fn write(level: Level, args: std::fmt::Arguments<'_>) {
    if !enabled(level) {
        return;
    }
    use std::io::Write;
    let mut stderr = std::io::stderr().lock();
    // Ignore write errors: a closed stderr must never crash the server.
    let _ = writeln!(stderr, "[redmine-mcp:{}] {}", level.as_str(), args);
}

#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => {
        $crate::log::write($crate::log::Level::Error, format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => {
        $crate::log::write($crate::log::Level::Warn, format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        $crate::log::write($crate::log::Level::Info, format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_debug {
    ($($arg:tt)*) => {
        $crate::log::write($crate::log::Level::Debug, format_args!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_levels() {
        assert_eq!(Level::parse("OFF"), Some(Level::Off));
        assert_eq!(Level::parse("warn"), Some(Level::Warn));
        assert_eq!(Level::parse(" Debug "), Some(Level::Debug));
        assert_eq!(Level::parse("verbose"), None);
    }

    #[test]
    fn level_order_is_monotonic() {
        assert!(Level::Off < Level::Error);
        assert!(Level::Error < Level::Warn);
        assert!(Level::Warn < Level::Info);
        assert!(Level::Info < Level::Debug);
    }
}
