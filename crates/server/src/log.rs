//! Access log sink: text or JSON lines on stdout, stderr or an appended file.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rxh_core::config::{Log, LogFormat};
use serde_json::Value;

enum Sink {
    Stdout,
    Stderr,
    File(Mutex<File>),
}

/// Writes access lines according to the `[log]` table.
pub(crate) struct Logger {
    format: LogFormat,
    access: bool,
    sink: Sink,
    write_failed: AtomicBool,
}

impl std::fmt::Debug for Logger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Logger")
            .field("format", &self.format)
            .field("access", &self.access)
            .finish_non_exhaustive()
    }
}

impl Logger {
    /// Opens the output of `config`, creating the file if needed.
    pub(crate) fn new(config: &Log) -> io::Result<Self> {
        let sink = match config.output.as_str() {
            "stdout" => Sink::Stdout,
            "stderr" => Sink::Stderr,
            path => {
                let file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .map_err(|err| io::Error::new(err.kind(), format!("log file {path}: {err}")))?;
                Sink::File(Mutex::new(file))
            }
        };
        Ok(Self {
            format: config.format,
            access: config.access,
            sink,
            write_failed: AtomicBool::new(false),
        })
    }

    /// Writes an access line: `text` in text format, `json` in JSON format.
    pub(crate) fn access(&self, text: impl FnOnce() -> String, json: impl FnOnce() -> Value) {
        match self.format {
            LogFormat::Text => self.access_text(text),
            LogFormat::Json => self.access_json(json),
        }
    }

    /// Writes an access line in text format only.
    pub(crate) fn access_text(&self, text: impl FnOnce() -> String) {
        if self.access && self.format == LogFormat::Text {
            self.write(&text());
        }
    }

    /// Writes an access line in JSON format only.
    pub(crate) fn access_json(&self, json: impl FnOnce() -> Value) {
        if self.access && self.format == LogFormat::Json {
            self.write(&json().to_string());
        }
    }

    /// True when access lines are written as JSON.
    pub(crate) fn is_json(&self) -> bool {
        self.access && self.format == LogFormat::Json
    }

    /// True when access lines are written in text format.
    pub(crate) fn is_text(&self) -> bool {
        self.format == LogFormat::Text
    }

    fn write(&self, line: &str) {
        let line = format!("{line}\n");
        // A broken log output must not take requests down: report the first
        // failure and keep going.
        let result = match &self.sink {
            Sink::Stdout => io::stdout().lock().write_all(line.as_bytes()),
            Sink::Stderr => io::stderr().lock().write_all(line.as_bytes()),
            Sink::File(file) => file
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .write_all(line.as_bytes()),
        };
        if let Err(err) = result
            && !self.write_failed.swap(true, Ordering::Relaxed)
        {
            eprintln!("Log => writing access log failed: {err}");
        }
    }
}

/// What a layer 4 listener needs to report its connections.
#[derive(Debug, Clone)]
pub(crate) struct ListenerLog {
    /// Name used in operational messages, like `127.0.0.1:80 (web)`.
    pub(crate) log_name: Arc<str>,

    /// Server name or listen address, for metrics and JSON lines.
    pub(crate) server: Arc<str>,

    pub(crate) logger: Arc<Logger>,
}

/// Formats `time` as RFC 3339 in UTC with milliseconds, like
/// `2024-02-29T13:05:09.042Z`.
pub(crate) fn rfc3339(time: SystemTime) -> String {
    let since_epoch = time.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    let seconds = since_epoch.as_secs();
    let (days, rest) = (seconds / 86_400, seconds % 86_400);

    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let shifted = days as i64 + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);

    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rest / 3_600,
        rest % 3_600 / 60,
        rest % 60,
        since_epoch.subsec_millis()
    )
}

/// Current time as RFC 3339.
pub(crate) fn now() -> String {
    rfc3339(SystemTime::now())
}

/// Milliseconds with microsecond precision, for `duration_ms`.
pub(crate) fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: u64, millis: u64) -> String {
        rfc3339(UNIX_EPOCH + Duration::from_millis(seconds * 1_000 + millis))
    }

    #[test]
    fn formats_known_timestamps() {
        assert_eq!(at(0, 0), "1970-01-01T00:00:00.000Z");
        assert_eq!(at(951_782_400, 0), "2000-02-29T00:00:00.000Z");
        assert_eq!(at(1_700_000_000, 123), "2023-11-14T22:13:20.123Z");
        assert_eq!(at(1_709_211_909, 42), "2024-02-29T13:05:09.042Z");
        assert_eq!(at(4_102_444_799, 999), "2099-12-31T23:59:59.999Z");
    }
}
