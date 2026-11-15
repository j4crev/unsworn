//! A project's files as a bundle holds them: a map from path to contents.

use std::collections::BTreeMap;

use unsworn_core::project::Host;

/// The project files a bundle carries, keyed by project-relative path.
/// `None` marks a file that exists but whose contents loading never reads,
/// such as a handler module.
#[derive(Debug, Default, Clone)]
pub struct Files {
    pub files: BTreeMap<String, Option<String>>,
    pub env: BTreeMap<String, String>,
}

fn normalize(path: &str) -> &str {
    let path = path.strip_prefix("./").unwrap_or(path);
    path.trim_end_matches('/')
}

impl Host for Files {
    fn read(&self, path: &str) -> Option<String> {
        self.files.get(normalize(path)).cloned().flatten()
    }

    fn exists(&self, path: &str) -> bool {
        let path = normalize(path);
        if path.is_empty() || path == "." || self.files.contains_key(path) {
            return true;
        }
        let dir = format!("{path}/");
        self.files
            .range(dir.clone()..)
            .next()
            .is_some_and(|(k, _)| k.starts_with(&dir))
    }

    fn list(&self, dir: &str) -> Result<Vec<String>, String> {
        let dir = normalize(dir);
        let prefix = if dir.is_empty() || dir == "." {
            String::new()
        } else {
            format!("{dir}/")
        };
        let out: Vec<String> = self
            .files
            .keys()
            .filter_map(|k| k.strip_prefix(&prefix).map(String::from))
            .collect();
        if out.is_empty() {
            return Err("not a directory".into());
        }
        Ok(out)
    }

    fn env(&self, key: &str) -> Option<String> {
        self.env.get(key).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files() -> Files {
        let mut f = Files::default();
        for (k, v) in [
            ("unsworn.toml", Some("x")),
            ("routes/users/get.ts", None),
            ("routes/users/_policy.toml", Some("y")),
            ("routes-other/a.ts", None),
        ] {
            f.files.insert(k.into(), v.map(String::from));
        }
        f
    }

    #[test]
    fn reads_and_lists() {
        let f = files();
        assert_eq!(f.read("./unsworn.toml").as_deref(), Some("x"));
        assert_eq!(f.read("routes/users/get.ts"), None);
        assert!(f.exists("routes/users/get.ts"));
        assert!(f.exists("routes"));
        assert!(f.exists("routes/"));
        assert!(!f.exists("route"));
        assert!(!f.exists("missing.ts"));
        assert_eq!(
            f.list("routes").unwrap(),
            vec!["users/_policy.toml", "users/get.ts"]
        );
        assert!(f.list("nope").is_err());
    }
}
