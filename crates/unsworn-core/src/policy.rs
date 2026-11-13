//! Resolved policies, layers and how they merge.
//!
//! Policies come from several layers, merged from broadest to most
//! specific: `[defaults]`, the OpenAPI spec, `_policies.toml` files, prefix
//! entries, then exact entries. Each layer only sets the keys it mentions.

use std::time::Duration;

use serde::Serialize;

use crate::config::OrOff;

/// A reference to a handler: `<file>[#<name>]`, plus the runtime that runs it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct HandlerRef {
    /// The reference exactly as passed to the worker.
    pub raw: String,
    /// Project-relative file path.
    pub file: String,
    pub name: Option<String>,
    pub runtime: String,
}

impl HandlerRef {
    /// Splits `file#name`. Returns `(file, name)`.
    pub fn split(raw: &str) -> (&str, Option<&str>) {
        match raw.split_once('#') {
            Some((file, name)) if !name.is_empty() => (file, Some(name)),
            Some((file, _)) => (file, None),
            None => (raw, None),
        }
    }
}

/// Which credentials a route accepts.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthReq {
    /// No authentication (`auth = "none"`, or not set).
    #[default]
    Public,
    /// Any one of these schemes, each with its required scopes.
    AnyOf(Vec<AuthAlt>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuthAlt {
    pub scheme: String,
    pub scopes: Vec<String>,
}

/// Where a rate-limit or cache key comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeySource {
    Ip,
    /// A claim from the verified credentials, such as `auth.sub`.
    Claim(String),
    /// A request header, such as `header.x-tenant`.
    Header(String),
}

impl KeySource {
    pub fn parse(s: &str) -> Result<KeySource, String> {
        if s == "ip" {
            Ok(KeySource::Ip)
        } else if let Some(claim) = s.strip_prefix("auth.") {
            Ok(KeySource::Claim(claim.to_string()))
        } else if let Some(h) = s.strip_prefix("header.") {
            Ok(KeySource::Header(h.to_ascii_lowercase()))
        } else {
            Err(format!(
                "\"{s}\" isn't a valid key: use \"ip\", \"auth.<claim>\" or \"header.<name>\""
            ))
        }
    }

    pub fn describe(&self) -> String {
        match self {
            KeySource::Ip => "ip".into(),
            KeySource::Claim(c) => format!("auth.{c}"),
            KeySource::Header(h) => format!("header.{h}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RateLimit {
    pub requests: u32,
    pub per: Duration,
    pub key: KeySource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CacheRule {
    pub ttl: Duration,
    /// Lowercase request header names.
    pub vary: Vec<String>,
    pub key: Option<KeySource>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Cors {
    /// `None` means any origin (`"*"`).
    pub origins: Option<Vec<String>>,
    pub credentials: bool,
    pub methods: Option<Vec<String>>,
    pub headers: Option<Vec<String>>,
    pub expose: Vec<String>,
    pub max_age: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct HeaderRules {
    pub set: Vec<(String, String)>,
    pub remove: Vec<String>,
}

/// One layer of policies. `None` means "not set by this layer".
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Layer {
    pub auth: Option<AuthReq>,
    pub cors: Option<OrOff<Cors>>,
    pub origin: Option<OrOff<Vec<String>>>,
    pub rate_limit: Option<OrOff<RateLimit>>,
    pub cache: Option<OrOff<CacheRule>>,
    pub timeout: Option<Duration>,
    pub max_body: Option<u64>,
    pub headers: Option<OrOff<HeaderRules>>,
    /// Accumulates across layers; `Off` clears what came before.
    pub before: Option<OrOff<Vec<HandlerRef>>>,
}

impl Layer {
    /// Applies `other` on top of `self`: keys `other` sets win.
    pub fn merge_from(&mut self, other: &Layer) {
        fn take<T: Clone>(slot: &mut Option<T>, value: &Option<T>) {
            if let Some(v) = value {
                *slot = Some(v.clone());
            }
        }
        take(&mut self.auth, &other.auth);
        take(&mut self.cors, &other.cors);
        take(&mut self.origin, &other.origin);
        take(&mut self.rate_limit, &other.rate_limit);
        take(&mut self.cache, &other.cache);
        take(&mut self.timeout, &other.timeout);
        take(&mut self.max_body, &other.max_body);
        take(&mut self.headers, &other.headers);
        match (&mut self.before, &other.before) {
            (_, None) => {}
            (slot, Some(OrOff::Off)) => *slot = Some(OrOff::Off),
            (Some(OrOff::On(existing)), Some(OrOff::On(more))) => {
                for h in more {
                    if !existing.contains(h) {
                        existing.push(h.clone());
                    }
                }
            }
            (slot, Some(OrOff::On(more))) => *slot = Some(OrOff::On(more.clone())),
        }
    }

    pub fn is_empty(&self) -> bool {
        self == &Layer::default()
    }

    /// Turns the merged layer into final settings.
    pub fn finalize(&self, default_timeout: Duration, default_max_body: u64) -> Effective {
        fn on<T: Clone>(v: &Option<OrOff<T>>) -> Option<T> {
            match v {
                Some(OrOff::On(t)) => Some(t.clone()),
                _ => None,
            }
        }
        Effective {
            auth: self.auth.clone().unwrap_or_default(),
            cors: on(&self.cors),
            origin: on(&self.origin),
            rate_limit: on(&self.rate_limit),
            cache: on(&self.cache),
            timeout: self.timeout.unwrap_or(default_timeout),
            max_body: self.max_body.unwrap_or(default_max_body),
            headers: on(&self.headers).unwrap_or_default(),
            before: on(&self.before).unwrap_or_default(),
        }
    }
}

/// The final policies for one request.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Effective {
    pub auth: AuthReq,
    pub cors: Option<Cors>,
    pub origin: Option<Vec<String>>,
    pub rate_limit: Option<RateLimit>,
    pub cache: Option<CacheRule>,
    pub timeout: Duration,
    pub max_body: u64,
    pub headers: HeaderRules,
    pub before: Vec<HandlerRef>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn href(name: &str) -> HandlerRef {
        HandlerRef {
            raw: name.into(),
            file: name.into(),
            name: None,
            runtime: "ts".into(),
        }
    }

    #[test]
    fn later_layers_win_and_before_accumulates() {
        let mut acc = Layer {
            timeout: Some(Duration::from_secs(10)),
            auth: Some(AuthReq::AnyOf(vec![AuthAlt {
                scheme: "jwt".into(),
                scopes: vec![],
            }])),
            before: Some(OrOff::On(vec![href("a.ts")])),
            ..Default::default()
        };
        acc.merge_from(&Layer {
            auth: Some(AuthReq::Public),
            before: Some(OrOff::On(vec![href("b.ts"), href("a.ts")])),
            ..Default::default()
        });
        let eff = acc.finalize(Duration::from_secs(30), 1024);
        assert_eq!(eff.auth, AuthReq::Public);
        assert_eq!(eff.timeout, Duration::from_secs(10));
        assert_eq!(eff.before, vec![href("a.ts"), href("b.ts")]);

        acc.merge_from(&Layer {
            before: Some(OrOff::Off),
            cache: Some(OrOff::Off),
            ..Default::default()
        });
        let eff = acc.finalize(Duration::from_secs(30), 1024);
        assert!(eff.before.is_empty());
        assert!(eff.cache.is_none());
    }

    #[test]
    fn handler_ref_split() {
        assert_eq!(HandlerRef::split("a.ts#b"), ("a.ts", Some("b")));
        assert_eq!(HandlerRef::split("a.ts"), ("a.ts", None));
        assert_eq!(HandlerRef::split("a.ts#"), ("a.ts", None));
    }

    #[test]
    fn key_sources() {
        assert_eq!(KeySource::parse("ip").unwrap(), KeySource::Ip);
        assert_eq!(
            KeySource::parse("auth.sub").unwrap(),
            KeySource::Claim("sub".into())
        );
        assert!(KeySource::parse("user").is_err());
    }
}
