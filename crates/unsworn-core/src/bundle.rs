//! A project as `unsworn build` bundles it, for a host with no filesystem:
//! an edge isolate or a serverless function.
//!
//! A bundle carries the files the compiler reads, and the core compiles
//! them again each time it starts. So that the core there needs no TOML or
//! YAML parser (the `parsers` feature, about a sixth of the WebAssembly
//! module), [`pack`] rewrites every config document as JSON. The files
//! keep their names, so the config can still say `spec = "openapi.yaml"`,
//! and [`Files`] reads each of them as JSON whatever its name.
//!
//! Converting must not change what the project enforces, and it can: YAML
//! takes an unquoted `8080` as text where the config expects text, and
//! JSON doesn't. So [`pack`] compiles the converted files as well and
//! compares the two plans, and reports what it finds instead of writing a
//! bundle that would fail, or act differently, once deployed.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::config::{Format, RawConfig, parse_document};
use crate::diag::Diagnostic;
use crate::plan::{Plan, diff};
use crate::project::{Compiled, Host, compile_symbolic};

/// The project files a bundle carries, keyed by project-relative path.
/// `None` marks a file that exists but whose contents loading never reads,
/// such as a handler module. Every config document among them is JSON.
#[derive(Debug, Default, Clone)]
pub struct Files {
    pub files: BTreeMap<String, Option<String>>,
    pub env: BTreeMap<String, String>,
}

/// A path as a bundle's keys have it: no `./` in front, no `/` after.
pub fn normalize(path: &str) -> &str {
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

    fn format(&self, _path: &str) -> Format {
        Format::Json
    }
}

/// Turns the files a compile read into a bundle's: every document in
/// `documents` (each path the compiler asked [`Host::format`] about, with
/// the answer) is rewritten as JSON. Then checks that the result loads
/// into the same project as `source`, which holds the files as written.
///
/// An error means the bundle must not be written. Each says which file
/// and value is the cause, where that can be told.
pub fn pack(
    source: &dyn Host,
    config_file: &str,
    mut files: BTreeMap<String, Option<String>>,
    documents: &BTreeMap<String, Format>,
) -> Result<BTreeMap<String, Option<String>>, Vec<Diagnostic>> {
    let mut errors = Vec::new();
    let mut converted: Vec<(&str, Format)> = Vec::new();
    for (path, format) in documents {
        if *format == Format::Json {
            continue;
        }
        let Some(Some(text)) = files.get(path) else {
            continue;
        };
        match parse_document::<Value>(text, *format) {
            Ok(value) => {
                files.insert(path.clone(), Some(value.to_string()));
                converted.push((path, *format));
            }
            Err(e) => errors.push(
                Diagnostic::error(path, format!("this can't be written as JSON: {e}")).hint(
                    "`unsworn build` stores config as JSON in the bundle, so each value has to be one JSON can hold",
                ),
            ),
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    if converted.is_empty() {
        return Ok(files);
    }
    let bundle = Files {
        files,
        env: BTreeMap::new(),
    };
    let errors = verify(source, &bundle, config_file, &converted);
    if errors.is_empty() {
        Ok(bundle.files)
    } else {
        Err(errors)
    }
}

fn name(format: Format) -> &'static str {
    match format {
        Format::Toml => "TOML",
        Format::Yaml => "YAML",
        Format::Json => "JSON",
    }
}

fn errors_of(compiled: &Compiled) -> Vec<Diagnostic> {
    compiled
        .diagnostics
        .iter()
        .filter(|d| d.is_error())
        .cloned()
        .collect()
}

/// What differs between the project in `source` and the one in `bundle`.
/// Neither compile reads the environment: a rule is the same wherever a
/// variable's value comes from.
fn verify(
    source: &dyn Host,
    bundle: &Files,
    config_file: &str,
    converted: &[(&str, Format)],
) -> Vec<Diagnostic> {
    // The config file is the one document read into typed settings, so
    // it's where a format's leniency shows. Checked on its own first,
    // because its error can be traced to a value.
    let config = normalize(config_file);
    if let Some((_, format)) = converted.iter().find(|(path, _)| *path == config)
        && let Some(text) = bundle.read(config)
        && let Some(found) = misread(&text, config_file, *format)
    {
        return vec![found];
    }

    let written = compile_symbolic(source, config_file);
    let Some(written_project) = &written.project else {
        return errors_of(&written);
    };
    let packed = compile_symbolic(bundle, config_file);
    let Some(packed_project) = &packed.project else {
        let formats: Vec<&str> = {
            let mut names: Vec<&str> = converted.iter().map(|(_, f)| name(*f)).collect();
            names.sort_unstable();
            names.dedup();
            names
        };
        return errors_of(&packed)
            .into_iter()
            .map(|d| {
                Diagnostic::error(
                    d.location,
                    format!(
                        "{} (once the project's {} is stored as JSON, as `unsworn build` does)",
                        d.message,
                        formats.join(" and ")
                    ),
                )
                .hint(format!(
                    "the files load as written, so a value reads differently as JSON; if it's a number or true/false that's meant as text, put it in quotes. Otherwise please report it: {}/issues",
                    env!("CARGO_PKG_REPOSITORY")
                ))
            })
            .collect();
    };

    let (before, after) = (Plan::of(written_project), Plan::of(packed_project));
    if before.render() == after.render() {
        return Vec::new();
    }
    let changes = diff(&before, &after).render();
    vec![
        Diagnostic::error(
            config_file,
            format!(
                "the bundle would enforce different rules than these files do:\n{}",
                changes.trim_end()
            ),
        )
        .hint(format!(
            "`unsworn build` stores config as JSON, and that shouldn't change a rule. Please report this with the lines above: {}/issues",
            env!("CARGO_PKG_REPOSITORY")
        )),
    ]
}

/// The error for a config file that loads as written and not as `json`,
/// its contents converted, with the value that's the cause.
fn misread(json: &str, config_file: &str, format: Format) -> Option<Diagnostic> {
    let error = serde_json::from_str::<RawConfig>(json).err()?;
    let value: Value = serde_json::from_str(json).ok()?;
    // `json` is one line, so the column is an offset into it. For a value
    // of the wrong type it's the value's last byte.
    let (path, found) = locate(&value, error.column().saturating_sub(1));
    let location = if path.is_empty() {
        config_file.to_string()
    } else {
        format!("{config_file} {path}")
    };
    let full = error.to_string();
    let message = full
        .rfind(" at line ")
        .map_or(full.as_str(), |end| &full[..end]);
    let kind = match found {
        Value::Number(_) => Some("a number"),
        Value::Bool(_) => Some("true or false"),
        _ => None,
    };
    let format = name(format);
    Some(match kind {
        Some(kind) if message.ends_with("expected a string") => Diagnostic::error(
            location,
            format!("{found} is {kind}, and this setting is text"),
        )
        .hint(format!(
            "write it in quotes: \"{found}\". {format} takes it as text either way, but `unsworn build` stores the config as JSON, where it has to be quoted",
        )),
        _ => Diagnostic::error(
            location,
            format!(
                "{message} (once this file is stored as JSON, as `unsworn build` does)"
            ),
        )
        .hint(format!(
            "the file loads as {format}, so this value reads differently as JSON. Write it the way JSON would hold it, such as in quotes if it's text",
        )),
    })
}

/// The innermost value whose text covers byte `at` of `value` written as
/// compact JSON, and its path from the top: `server.listen`,
/// `route[2].respond.text`. Lists count from one, as `route[2]` does in
/// other diagnostics.
fn locate(value: &Value, at: usize) -> (String, &Value) {
    let mut path = String::new();
    let mut current = value;
    // Where `current` starts.
    let mut start = 0;
    'down: loop {
        // Past the opening bracket.
        let mut pos = start + 1;
        match current {
            Value::Object(map) => {
                for (key, child) in map {
                    pos += Value::from(key.as_str()).to_string().len() + 1;
                    let len = child.to_string().len();
                    if (pos..pos + len).contains(&at) {
                        if !path.is_empty() {
                            path.push('.');
                        }
                        path.push_str(key);
                        (current, start) = (child, pos);
                        continue 'down;
                    }
                    pos += len + 1;
                }
            }
            Value::Array(list) => {
                for (i, child) in list.iter().enumerate() {
                    let len = child.to_string().len();
                    if (pos..pos + len).contains(&at) {
                        path.push_str(&format!("[{}]", i + 1));
                        (current, start) = (child, pos);
                        continue 'down;
                    }
                    pos += len + 1;
                }
            }
            _ => {}
        }
        return (path, current);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(list: &[(&str, Option<&str>)]) -> Files {
        Files {
            files: list
                .iter()
                .map(|(k, v)| (k.to_string(), v.map(String::from)))
                .collect(),
            env: BTreeMap::new(),
        }
    }

    #[test]
    fn reads_and_lists() {
        let f = files(&[
            ("unsworn.toml", Some("x")),
            ("routes/users/get.ts", None),
            ("routes/users/_policy.toml", Some("y")),
            ("routes-other/a.ts", None),
        ]);
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
        assert_eq!(f.format("unsworn.toml"), Format::Json);
    }

    /// A host that holds files as written and notes which were parsed, as
    /// `unsworn build`'s does.
    struct Written {
        files: Files,
        documents: std::cell::RefCell<BTreeMap<String, Format>>,
    }

    impl Host for Written {
        fn read(&self, path: &str) -> Option<String> {
            self.files.read(path)
        }
        fn exists(&self, path: &str) -> bool {
            self.files.exists(path)
        }
        fn list(&self, dir: &str) -> Result<Vec<String>, String> {
            self.files.list(dir)
        }
        fn env(&self, _key: &str) -> Option<String> {
            None
        }
        fn format(&self, path: &str) -> Format {
            let format = Format::from_path(path);
            self.documents
                .borrow_mut()
                .insert(normalize(path).to_string(), format);
            format
        }
    }

    /// Compiles `list` as written, then packs it.
    fn packed(
        config: &str,
        list: &[(&str, Option<&str>)],
    ) -> Result<BTreeMap<String, Option<String>>, Vec<Diagnostic>> {
        let source = Written {
            files: files(list),
            documents: Default::default(),
        };
        let compiled = compile_symbolic(&source, config);
        assert!(
            compiled.project.is_some(),
            "the project should load as written: {:?}",
            compiled.diagnostics
        );
        let documents = source.documents.take();
        pack(&source, config, source.files.files.clone(), &documents)
    }

    const RUNTIME: &str =
        "[runtimes.ts]\nextensions = [\".ts\"]\ncommand = \"bun run worker.ts\"\n";

    #[test]
    fn every_document_becomes_json_and_other_files_stay() {
        let config = format!(
            r#"{RUNTIME}
[routes]
dir = "routes"

[auth.jwt]
type = "jwt"
public_key = "keys/jwt.pem"

[openapi]
spec = "openapi.yaml"
handler = "src/{{operationId}}.ts"

[defaults]
rate_limit = {{ requests = 60, per = "1m" }}

[[route]]
path = "/users"
method = "POST"
handler = "src/users.ts#create"
body = "schemas/user.yaml"
auth = "jwt"

[[route]]
path = "/up/{{*rest}}"
proxy = {{ upstreams = ["{{env.UPSTREAM}}"] }}
"#
        );
        let spec = "openapi: 3.0.3\ninfo: { title: T, version: 1.0.0 }\npaths:\n  /pets:\n    get:\n      operationId: listPets\n      responses:\n        200: { description: ok }\n";
        let list = [
            ("unsworn.toml", Some(config.as_str())),
            ("keys/jwt.pem", Some("-----BEGIN PUBLIC KEY-----\nabc\n")),
            ("openapi.yaml", Some(spec)),
            (
                "schemas/user.yaml",
                Some("type: object\nrequired: [name]\nproperties:\n  name: { type: string }\n"),
            ),
            (
                "routes/_policies.toml",
                Some("cors = { origins = [\"https://a.example\"] }\n"),
            ),
            ("routes/api.items.get.ts", None),
            ("src/users.ts", None),
            ("src/listPets.ts", None),
        ];
        let out = packed("unsworn.toml", &list).unwrap();

        // Read as it is, so left as it is.
        assert_eq!(
            out["keys/jwt.pem"].as_deref(),
            Some("-----BEGIN PUBLIC KEY-----\nabc\n")
        );
        assert_eq!(out["src/users.ts"], None);
        for doc in [
            "unsworn.toml",
            "openapi.yaml",
            "schemas/user.yaml",
            "routes/_policies.toml",
        ] {
            let text = out[doc].as_deref().unwrap();
            assert!(
                serde_json::from_str::<Value>(text).is_ok(),
                "{doc} isn't JSON: {text}"
            );
        }
        // A variable is still there to read at runtime.
        assert!(
            out["unsworn.toml"]
                .as_deref()
                .unwrap()
                .contains("{env.UPSTREAM}")
        );

        // And the bundle is the same project.
        let bundle = Files {
            files: out,
            env: BTreeMap::new(),
        };
        let from_bundle = compile_symbolic(&bundle, "unsworn.toml");
        let as_written = compile_symbolic(
            &Written {
                files: files(&list),
                documents: Default::default(),
            },
            "unsworn.toml",
        );
        assert_eq!(
            Plan::of(&from_bundle.project.unwrap()).render(),
            Plan::of(&as_written.project.unwrap()).render()
        );
    }

    #[test]
    fn json_documents_are_left_as_written() {
        let config =
            "{\n  \"route\": [{ \"path\": \"/health\", \"respond\": { \"status\": 200 } }]\n}\n";
        let out = packed("unsworn.json", &[("unsworn.json", Some(config))]).unwrap();
        assert_eq!(out["unsworn.json"].as_deref(), Some(config));
    }

    #[test]
    fn yaml_text_without_quotes_is_named() {
        // A runtime's `env` is text, and a port number is the usual thing
        // to put there.
        let config = "runtimes:\n  ts:\n    extensions: [.ts]\n    command: bun run worker.ts\n    env:\n      PORT: 3000\nroute:\n  - path: /health\n    respond: { status: 200 }\n";
        let errors = packed("unsworn.yaml", &[("unsworn.yaml", Some(config))]).unwrap_err();
        assert_eq!(errors.len(), 1, "{errors:?}");
        let text = errors[0].to_string();
        assert_eq!(
            text,
            "✗ unsworn.yaml runtimes.ts.env.PORT: 3000 is a number, and this setting is text\n  → write it in quotes: \"3000\". YAML takes it as text either way, but `unsworn build` stores the config as JSON, where it has to be quoted"
        );
    }

    #[test]
    fn a_value_inside_a_list_is_found() {
        let config = "route:\n  - path: /a\n    respond: { status: 200 }\n  - path: /b\n    respond: { status: 200, text: true }\n";
        let errors = packed("unsworn.yaml", &[("unsworn.yaml", Some(config))]).unwrap_err();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].location, "unsworn.yaml route[2].respond.text");
        assert_eq!(
            errors[0].message,
            "true is true or false, and this setting is text"
        );
    }

    #[test]
    fn quoted_yaml_packs() {
        let config = "runtimes:\n  ts:\n    extensions: [.ts]\n    command: bun run worker.ts\n    env:\n      PORT: \"3000\"\nopenapi:\n  title: yes\n  version: \"1.0\"\nroute:\n  - path: /health\n    respond: { status: 200, text: \"12\" }\n";
        let out = packed("unsworn.yaml", &[("unsworn.yaml", Some(config))]).unwrap();
        assert!(out["unsworn.yaml"].as_deref().unwrap().starts_with('{'));
    }

    // The two checks behind the first. No config is known to trip them,
    // so each gets a bundle that was changed by hand.

    #[test]
    fn a_bundle_that_does_not_load_is_refused() {
        let config = "[routes]\ndir = \"routes\"\n";
        let policies = "rate_limit = { requests = 60, per = \"1m\" }\n";
        let source = Written {
            files: files(&[
                ("unsworn.toml", Some(config)),
                ("routes/_policies.toml", Some(policies)),
            ]),
            documents: Default::default(),
        };
        let bundle = files(&[
            ("unsworn.toml", Some(r#"{"routes":{"dir":"routes"}}"#)),
            (
                "routes/_policies.toml",
                Some(r#"{"rate_limit":{"requests":"sixty","per":"1m"}}"#),
            ),
        ]);
        let converted = [
            ("unsworn.toml", Format::Toml),
            ("routes/_policies.toml", Format::Toml),
        ];
        let errors = verify(&source, &bundle, "unsworn.toml", &converted);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].location, "routes/_policies.toml");
        assert!(
            errors[0]
                .message
                .ends_with("(once the project's TOML is stored as JSON, as `unsworn build` does)"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn a_bundle_with_other_rules_is_refused() {
        let config = "[auth.key]\ntype = \"api_key\"\nkeys = [\"{env.KEY}\"]\n\n[[route]]\npath = \"/admin\"\nrespond = { status = 200 }\nauth = \"key\"\n";
        let source = Written {
            files: files(&[("unsworn.toml", Some(config))]),
            documents: Default::default(),
        };
        let bundle = files(&[(
            "unsworn.toml",
            Some(
                r#"{"auth":{"key":{"type":"api_key","keys":["{env.KEY}"]}},"route":[{"path":"/admin","respond":{"status":200}}]}"#,
            ),
        )]);
        let errors = verify(
            &source,
            &bundle,
            "unsworn.toml",
            &[("unsworn.toml", Format::Toml)],
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
        let text = errors[0].to_string();
        assert!(
            text.starts_with(
                "✗ unsworn.toml: the bundle would enforce different rules than these files do:\n"
            ),
            "{text}"
        );
        assert!(text.contains("/admin"), "{text}");
        assert!(text.contains("(looser)"), "{text}");
    }

    #[test]
    fn locates_values_by_offset() {
        let value: Value =
            serde_json::from_str(r#"{"a":{"b":[10,{"c d":"x"}],"e":true},"f":null}"#).unwrap();
        let text = value.to_string();
        let at = |needle: &str| text.find(needle).unwrap();
        assert_eq!(locate(&value, at("10")).0, "a.b[1]");
        assert_eq!(locate(&value, at("\"x\"") + 2).0, "a.b[2].c d");
        assert_eq!(locate(&value, at("true") + 3).0, "a.e");
        assert_eq!(locate(&value, at("null")).0, "f");
        // A key belongs to the object it's in.
        assert_eq!(locate(&value, at("\"e\"")).0, "a");
        assert_eq!(locate(&value, 0).0, "");
    }
}
