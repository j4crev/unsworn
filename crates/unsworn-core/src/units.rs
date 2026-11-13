//! Durations ("30s", "5m", "1h30m", "250ms") and sizes ("1MB", "512KB").

use std::fmt;
use std::time::Duration;

use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize, Serializer};

/// Parses a duration such as `250ms`, `30s`, `5m`, `1h`, `1d` or `1m30s`.
/// A bare number means seconds.
pub fn parse_duration(input: &str) -> Result<Duration, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("empty duration".into());
    }
    if let Ok(secs) = s.parse::<u64>() {
        return Ok(Duration::from_secs(secs));
    }
    let mut total = Duration::ZERO;
    let mut rest = s;
    while !rest.is_empty() {
        let digits = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .ok_or_else(|| format!("\"{input}\" is missing a unit (ms, s, m, h or d)"))?;
        if digits == 0 {
            return Err(format!(
                "\"{input}\" is not a duration like \"30s\" or \"5m\""
            ));
        }
        let number: f64 = rest[..digits]
            .parse()
            .map_err(|_| format!("\"{input}\" has an invalid number"))?;
        rest = &rest[digits..];
        let unit_end = rest
            .find(|c: char| c.is_ascii_digit())
            .unwrap_or(rest.len());
        let unit = &rest[..unit_end];
        rest = &rest[unit_end..];
        let secs = match unit {
            "ms" => number / 1000.0,
            "s" => number,
            "m" => number * 60.0,
            "h" => number * 3600.0,
            "d" => number * 86400.0,
            _ => {
                return Err(format!(
                    "\"{input}\" has an unknown unit \"{unit}\" (use ms, s, m, h or d)"
                ));
            }
        };
        total += Duration::from_secs_f64(secs);
    }
    Ok(total)
}

/// Parses a size such as `512B`, `64KB`, `1MB` or `2GB`. Units are
/// 1024-based and case-insensitive; a bare number means bytes.
pub fn parse_size(input: &str) -> Result<u64, String> {
    let s = input.trim();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (number, unit) = s.split_at(split);
    let number: f64 = number
        .parse()
        .map_err(|_| format!("\"{input}\" is not a size like \"1MB\" or \"512KB\""))?;
    let factor = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1u64,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        other => {
            return Err(format!(
                "\"{input}\" has an unknown unit \"{other}\" (use B, KB, MB or GB)"
            ));
        }
    };
    Ok((number * factor as f64) as u64)
}

/// A duration in config: a string like `"30s"` or a number of seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigDuration(pub Duration);

impl<'de> Deserialize<'de> for ConfigDuration {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = ConfigDuration;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a duration like \"30s\", \"5m\" or \"250ms\"")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                parse_duration(v).map(ConfigDuration).map_err(E::custom)
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(ConfigDuration(Duration::from_secs(v)))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
                u64::try_from(v)
                    .map(|v| ConfigDuration(Duration::from_secs(v)))
                    .map_err(|_| E::custom("a duration can't be negative"))
            }
        }
        d.deserialize_any(V)
    }
}

impl Serialize for ConfigDuration {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format_duration(self.0))
    }
}

pub fn format_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if !ms.is_multiple_of(1000) {
        format!("{ms}ms")
    } else if ms.is_multiple_of(3_600_000) && ms > 0 {
        format!("{}h", ms / 3_600_000)
    } else if ms.is_multiple_of(60_000) && ms > 0 {
        format!("{}m", ms / 60_000)
    } else {
        format!("{}s", ms / 1000)
    }
}

impl schemars::JsonSchema for ConfigDuration {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Duration".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "description": "A duration like \"30s\", \"5m\", \"250ms\" or \"1h30m\", or a number of seconds.",
            "anyOf": [
                { "type": "string", "pattern": "^([0-9]+(\\.[0-9]+)?(ms|s|m|h|d))+$" },
                { "type": "integer", "minimum": 0 }
            ]
        })
    }
}

/// A size in config: a string like `"1MB"` or a number of bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigSize(pub u64);

impl<'de> Deserialize<'de> for ConfigSize {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = ConfigSize;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a size like \"1MB\" or \"512KB\"")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                parse_size(v).map(ConfigSize).map_err(E::custom)
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(ConfigSize(v))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
                u64::try_from(v)
                    .map(ConfigSize)
                    .map_err(|_| E::custom("a size can't be negative"))
            }
        }
        d.deserialize_any(V)
    }
}

impl schemars::JsonSchema for ConfigSize {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Size".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "description": "A size like \"1MB\" or \"512KB\" (1024-based), or a number of bytes.",
            "anyOf": [
                { "type": "string", "pattern": "^[0-9]+(\\.[0-9]+)?\\s*([kKmMgG][iI]?[bB]?|[bB])?$" },
                { "type": "integer", "minimum": 0 }
            ]
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("250ms").unwrap(), Duration::from_millis(250));
        assert_eq!(parse_duration("1m30s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse_duration("15").unwrap(), Duration::from_secs(15));
        assert!(parse_duration("5 minutes").is_err());
        assert!(parse_duration("s").is_err());
        assert_eq!(format_duration(Duration::from_secs(300)), "5m");
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("1MB").unwrap(), 1024 * 1024);
        assert_eq!(parse_size("512kb").unwrap(), 512 * 1024);
        assert_eq!(parse_size("100").unwrap(), 100);
        assert!(parse_size("1PB").is_err());
    }
}
