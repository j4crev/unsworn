//! The access log: one line for every request.
//!
//! It's an ordinary `tracing` event with the target `access`, so
//! `UNSWORN_LOG` turns it off or on like any other (`info,access=off`).
//! Since every request writes one, a server whose log isn't a terminal
//! puts the line together here instead, in the same format, and writes
//! lines a batch at a time: when a thread has [`BATCH`] bytes of them, and
//! every [`INTERVAL`] otherwise.

use std::fmt::Write as _;
use std::io::Write as _;
use std::net::IpAddr;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::support::{thread_slot, thread_slots};

/// A thread writes its lines out once it has this many bytes of them.
const BATCH: usize = 16 * 1024;

/// How long a line can wait to be written.
const INTERVAL: Duration = Duration::from_millis(100);

pub struct Entry<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub status: u16,
    pub route: &'a str,
    /// How long the request took, in milliseconds.
    pub ms: f64,
    pub ip: IpAddr,
    pub id: &'a str,
}

#[derive(Clone, Copy, PartialEq)]
enum Format {
    Text,
    Json,
}

struct Batched {
    format: Format,
    /// Lines waiting to be written, per thread (see [`thread_slot`]).
    waiting: Box<[Mutex<Vec<u8>>]>,
}

/// Set when lines are put together here. Otherwise they're `tracing`
/// events.
static BATCHED: OnceLock<Batched> = OnceLock::new();

/// Call once, after the `tracing` subscriber is set. `format` is the
/// subscriber's (`text` or `json`), and `plain` is whether it writes
/// timestamps and no colors: the output this module knows how to match.
pub fn init(format: &str, plain: bool) {
    if !plain || !tracing::enabled!(target: "access", tracing::Level::INFO) {
        return;
    }
    let batched = Batched {
        format: if format == "json" {
            Format::Json
        } else {
            Format::Text
        },
        waiting: (0..thread_slots()).map(|_| Mutex::default()).collect(),
    };
    if BATCHED.set(batched).is_ok() {
        let _ = std::thread::Builder::new()
            .name("access-log".into())
            .spawn(|| {
                loop {
                    std::thread::sleep(INTERVAL);
                    flush();
                }
            });
    }
}

/// Writes out every line that's waiting. Call before the process exits.
pub fn flush() {
    let Some(batched) = BATCHED.get() else {
        return;
    };
    for waiting in &batched.waiting {
        let lines = std::mem::take(&mut *waiting.lock().unwrap());
        if !lines.is_empty() {
            let _ = std::io::stderr().lock().write_all(&lines);
        }
    }
}

pub fn log(entry: &Entry<'_>) {
    let Some(batched) = BATCHED.get() else {
        tracing::info!(
            target: "access",
            method = %entry.method,
            path = %entry.path,
            status = entry.status,
            route = %entry.route,
            ms = entry.ms,
            ip = %entry.ip,
            id = %entry.id,
        );
        return;
    };
    let full = {
        let mut waiting = batched.waiting[thread_slot() % batched.waiting.len()]
            .lock()
            .unwrap();
        if waiting.capacity() == 0 {
            waiting.reserve(BATCH + 1024);
        }
        line(&mut waiting, batched.format, SystemTime::now(), entry);
        (waiting.len() >= BATCH).then(|| std::mem::take(&mut *waiting))
    };
    if let Some(lines) = full {
        let _ = std::io::stderr().lock().write_all(&lines);
    }
}

/// Appends the line `tracing` would write for the entry.
fn line(out: &mut Vec<u8>, format: Format, now: SystemTime, entry: &Entry<'_>) {
    let mut text = Text(out);
    match format {
        Format::Text => {
            timestamp(&mut text, now);
            let _ = writeln!(
                text,
                "  INFO access: method={} path={} status={} route={} ms={:?} ip={} id={}",
                entry.method, entry.path, entry.status, entry.route, entry.ms, entry.ip, entry.id
            );
        }
        Format::Json => {
            text.0.extend_from_slice(b"{\"timestamp\":\"");
            timestamp(&mut text, now);
            text.0
                .extend_from_slice(b"\",\"level\":\"INFO\",\"method\":");
            json(text.0, entry.method);
            text.0.extend_from_slice(b",\"path\":");
            json(text.0, entry.path);
            let _ = write!(text, ",\"status\":{},\"route\":", entry.status);
            json(text.0, entry.route);
            text.0.extend_from_slice(b",\"ms\":");
            json(text.0, &entry.ms);
            let _ = write!(text, ",\"ip\":\"{}\",\"id\":", entry.ip);
            json(text.0, entry.id);
            text.0.extend_from_slice(b",\"target\":\"access\"}\n");
        }
    }
}

fn json<T: serde::Serialize + ?Sized>(out: &mut Vec<u8>, value: &T) {
    let _ = serde_json::to_writer(out, value);
}

/// `fmt::Write` for bytes that are always given text.
struct Text<'a>(&'a mut Vec<u8>);

impl std::fmt::Write for Text<'_> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

/// `2026-11-17T07:00:00.123456Z`: UTC, to the microsecond.
fn timestamp(out: &mut Text<'_>, now: SystemTime) {
    thread_local! {
        /// The last second this thread wrote, and its text up to the seconds.
        static LAST: std::cell::Cell<(u64, [u8; 19])> = const { std::cell::Cell::new((u64::MAX, [0; 19])) };
    }
    let since = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since.as_secs();
    let (last, mut text) = LAST.get();
    if last != secs {
        text = to_the_second(secs);
        LAST.set((secs, text));
    }
    out.0.extend_from_slice(&text);
    let _ = write!(out, ".{:06}Z", since.subsec_micros());
}

/// `2026-11-17T07:00:00` for a time in seconds since 1970, for years up to
/// 9999.
fn to_the_second(secs: u64) -> [u8; 19] {
    let (days, time) = (secs / 86_400, secs % 86_400);
    // Days to a date, from Howard Hinnant's `civil_from_days`.
    let z = days + 719_468;
    let era = z / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    let mut text = *b"0000-00-00T00:00:00";
    let mut put = |at: usize, digits: usize, mut value: u64| {
        for i in (at..at + digits).rev() {
            text[i] = b'0' + (value % 10) as u8;
            value /= 10;
        }
    };
    put(0, 4, year);
    put(5, 2, month);
    put(8, 2, day);
    put(11, 2, time / 3_600);
    put(14, 2, time / 60 % 60);
    put(17, 2, time % 60);
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Collects what a subscriber writes.
    #[derive(Clone, Default)]
    struct Sink(std::sync::Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
        type Writer = Sink;
        fn make_writer(&'a self) -> Sink {
            self.clone()
        }
    }

    fn entries() -> Vec<Entry<'static>> {
        vec![
            Entry {
                method: "GET",
                path: "/",
                status: 200,
                route: "/{*rest}",
                ms: 0.123,
                ip: "127.0.0.1".parse().unwrap(),
                id: "1a2b-3f",
            },
            Entry {
                method: "POST",
                path: "/a \"quoted\" path/\u{e9}\\",
                status: 404,
                route: "(none)",
                ms: 12.0,
                ip: "2001:db8::7".parse().unwrap(),
                id: "from the client",
            },
            Entry {
                method: "DELETE",
                path: "/slow",
                status: 504,
                route: "/slow",
                ms: 0.0,
                ip: "10.1.2.3".parse().unwrap(),
                id: "x",
            },
            Entry {
                method: "GET",
                path: "/long",
                status: 200,
                route: "/long",
                ms: 123_456_789.25,
                ip: "10.1.2.3".parse().unwrap(),
                id: "y",
            },
        ]
    }

    /// What the subscriber `main` sets up writes for the entries.
    fn from_tracing(format: Format) -> String {
        let sink = Sink::default();
        let builder = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(sink.clone());
        let emit = || {
            for entry in entries() {
                tracing::info!(
                    target: "access",
                    method = %entry.method,
                    path = %entry.path,
                    status = entry.status,
                    route = %entry.route,
                    ms = entry.ms,
                    ip = %entry.ip,
                    id = %entry.id,
                );
            }
        };
        match format {
            Format::Text => tracing::subscriber::with_default(builder.finish(), emit),
            Format::Json => {
                tracing::subscriber::with_default(builder.json().flatten_event(true).finish(), emit)
            }
        }
        String::from_utf8(sink.0.lock().unwrap().clone()).unwrap()
    }

    /// The same lines with every timestamp replaced, since the two were
    /// written at different times.
    fn without_times(lines: &str, format: Format) -> String {
        let at = match format {
            Format::Text => 0,
            Format::Json => "{\"timestamp\":\"".len(),
        };
        let len = "2026-11-17T07:00:00.123456Z".len();
        lines
            .lines()
            .map(|l| format!("{}<time>{}\n", &l[..at], &l[at + len..]))
            .collect()
    }

    #[test]
    fn lines_match_what_tracing_writes() {
        for format in [Format::Text, Format::Json] {
            let mut ours = Vec::new();
            for entry in entries() {
                line(&mut ours, format, SystemTime::now(), &entry);
            }
            let ours = String::from_utf8(ours).unwrap();
            assert_eq!(
                without_times(&ours, format),
                without_times(&from_tracing(format), format)
            );
        }
    }

    #[test]
    fn timestamps_match_what_tracing_writes() {
        // A leap day, a year's last second and its first, and a time with
        // every field in two digits.
        for (secs, micros, want) in [
            (0, 0, "1970-01-01T00:00:00.000000Z"),
            (1_709_210_096, 7, "2024-02-29T12:34:56.000007Z"),
            (1_735_689_599, 999_999, "2024-12-31T23:59:59.999999Z"),
            (1_735_689_600, 0, "2025-01-01T00:00:00.000000Z"),
            (1_794_898_800, 123_456, "2026-11-17T07:00:00.123456Z"),
            (4_102_444_800, 1, "2100-01-01T00:00:00.000001Z"),
        ] {
            let now = UNIX_EPOCH + Duration::from_secs(secs) + Duration::from_micros(micros);
            let mut out = Vec::new();
            timestamp(&mut Text(&mut out), now);
            assert_eq!(String::from_utf8(out).unwrap(), want);
        }
    }
}
