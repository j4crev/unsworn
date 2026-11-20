//! File-based routes: nested or flat.
//!
//! A route is built from the directory names followed by the parts of the
//! file name split on `.`, where the last part before the extension is the
//! HTTP method. `[name]` is a parameter, `[...name]` matches the rest of the
//! path, `(name)` is a group that doesn't appear in the URL, and `[.]` is a
//! literal dot. A folder's policies and `before` handlers are in the
//! config, as an entry for the folder's path: a `_policies.toml` or
//! `_before.<ext>` file here is an error that says so.

pub const METHODS: &[&str] = &["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"];

/// A route defined by a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRoute {
    pub method: String,
    /// Route pattern, e.g. `/users/{id}`.
    pub path: String,
    /// Path of the file relative to the routes directory.
    pub file: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Scan {
    pub routes: Vec<FileRoute>,
}

/// Stands for the routes directory in an error from [`scan`] that gives
/// a file's path from the project, which only the caller knows.
pub const ROUTES_DIR: &str = "<routes>";

/// Classifies files from a routes directory. Paths are relative to the
/// directory and use `/`. Files that aren't routes, such as shared helper
/// modules, are ignored.
pub fn scan(files: &[String]) -> Result<Scan, Vec<(String, String)>> {
    let mut out = Scan::default();
    let mut errors = Vec::new();
    for file in files {
        match classify(file) {
            Ok(Some(r)) => out.routes.push(r),
            Ok(None) => {}
            Err(e) => errors.push((file.clone(), e)),
        }
    }
    out.routes
        .sort_by(|a, b| a.path.cmp(&b.path).then(a.method.cmp(&b.method)));
    if errors.is_empty() {
        Ok(out)
    } else {
        Err(errors)
    }
}

fn classify(file: &str) -> Result<Option<FileRoute>, String> {
    let mut components: Vec<&str> = file.split('/').filter(|c| !c.is_empty()).collect();
    let Some(name) = components.pop() else {
        return Ok(None);
    };
    if name.starts_with('.')
        || components
            .iter()
            .any(|c| c.starts_with('.') && *c != ".well-known")
    {
        return Ok(None);
    }
    let parts = split_name(name)?;
    if parts.len() < 2 {
        return Ok(None); // no extension
    }
    let (stem, _ext) = parts.split_at(parts.len() - 1);
    let Some((last, before_last)) = stem.split_last() else {
        return Ok(None);
    };
    let mut chain: Vec<String> = components.iter().map(|c| c.to_string()).collect();
    chain.extend(before_last.iter().cloned());

    if last == "_policies" {
        // It would cover the route files in its folder, but not a route
        // from the config or an OpenAPI spec under the same path, which
        // is easy to take it for.
        let folder = chain_to_path(&chain).unwrap_or_else(|_| "/<folder>".into());
        return Err(format!(
            "policies for a folder of route files aren't read from a file; put them in the config, as an entry for the folder's path: `[[route]]` with `path = \"{}/{{*rest}}\"`",
            folder.trim_end_matches('/')
        ));
    }
    if last == "_before" {
        // The same goes for a `before` handler picked up by its name: it
        // would run for the route files beside it, and not for another
        // route under the same path.
        let folder = chain_to_path(&chain).unwrap_or_else(|_| "/<folder>".into());
        // Listed under this name it would be this error again, so the
        // advice comes with another one.
        let renamed = match file.rfind("_before") {
            Some(at) => format!("{}guard{}", &file[..at], &file[at + "_before".len()..]),
            None => file.to_string(),
        };
        return Err(format!(
            "a `before` handler for a folder isn't picked up by its name; give the file another name and list it in the config, in an entry for the folder's path: `[[route]]` with `path = \"{}/{{*rest}}\"` and `before = [\"{ROUTES_DIR}/{renamed}\"]`",
            folder.trim_end_matches('/')
        ));
    }
    let method = last.to_ascii_uppercase();
    if !METHODS.contains(&method.as_str()) || last.chars().any(|c| c.is_ascii_uppercase()) {
        return Ok(None);
    }
    let path = chain_to_path(&chain)?;
    Ok(Some(FileRoute {
        method,
        path,
        file: file.to_string(),
    }))
}

/// Splits a file name on `.`, keeping dots inside `[...]` and turning
/// `[.]` into a literal dot.
fn split_name(name: &str) -> Result<Vec<String>, String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    for c in name.chars() {
        match c {
            '[' => {
                depth += 1;
                current.push(c);
            }
            ']' => {
                depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| format!("unbalanced `]` in \"{name}\""))?;
                current.push(c);
            }
            '.' if depth == 0 => parts.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    if depth != 0 {
        return Err(format!("unbalanced `[` in \"{name}\""));
    }
    parts.push(current);
    Ok(parts)
}

fn chain_to_path(chain: &[String]) -> Result<String, String> {
    let mut segments = Vec::new();
    for (i, part) in chain.iter().enumerate() {
        if part.starts_with('(') && part.ends_with(')') {
            continue;
        }
        segments.push(segment(part, i == chain.len() - 1)?);
    }
    Ok(format!("/{}", segments.join("/")))
}

fn segment(part: &str, is_last: bool) -> Result<String, String> {
    if let Some(inner) = part.strip_prefix("[...").and_then(|p| p.strip_suffix(']')) {
        check_param(inner)?;
        if !is_last {
            return Err(format!(
                "`[...{inner}]` matches the rest of the path, so it must be the last part"
            ));
        }
        return Ok(format!("{{*{inner}}}"));
    }
    if let Some(inner) = part.strip_prefix('[').and_then(|p| p.strip_suffix(']'))
        && inner != "."
    {
        check_param(inner)?;
        return Ok(format!("{{{inner}}}"));
    }
    let literal = part.replace("[.]", ".");
    if literal.contains('[') || literal.contains(']') {
        return Err(format!(
            "\"{part}\": brackets must wrap a whole part, like `[id]`"
        ));
    }
    Ok(literal.replace('{', "{{").replace('}', "}}"))
}

fn check_param(name: &str) -> Result<(), String> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(format!(
            "\"{name}\" isn't a valid parameter name (letters, digits and `_` only)"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(files: &[&str]) -> Vec<(String, String)> {
        let files: Vec<String> = files.iter().map(|s| s.to_string()).collect();
        scan(&files)
            .unwrap()
            .routes
            .into_iter()
            .map(|r| (r.method, r.path))
            .collect()
    }

    #[test]
    fn nested_and_flat_are_equivalent() {
        let nested = paths(&[
            "get.ts",
            "users/get.ts",
            "users/[id]/put.go",
            "files/[...path]/get.rs",
            "(admin)/stats/get.ts",
        ]);
        let flat = paths(&[
            "get.ts",
            "users.get.ts",
            "users.[id].put.go",
            "files.[...path].get.rs",
            "(admin).stats.get.ts",
        ]);
        assert_eq!(nested, flat);
        assert!(nested.contains(&("PUT".into(), "/users/{id}".into())));
        assert!(nested.contains(&("GET".into(), "/files/{*path}".into())));
        assert!(nested.contains(&("GET".into(), "/stats".into())));
        assert!(nested.contains(&("GET".into(), "/".into())));
    }

    #[test]
    fn literal_dots_and_well_known() {
        let routes = paths(&["sitemap[.]xml.get.ts", ".well-known/security[.]txt.get.ts"]);
        assert!(routes.contains(&("GET".into(), "/sitemap.xml".into())));
        assert!(routes.contains(&("GET".into(), "/.well-known/security.txt".into())));
    }

    #[test]
    fn helpers_are_ignored() {
        let files: Vec<String> = ["users/db.ts", "admin/stats.get.ts", ".hidden.get.ts"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let scan = scan(&files).unwrap();
        assert_eq!(scan.routes.len(), 1);
    }

    /// A file that used to apply to its folder's routes by its name is
    /// reported, with the entry that says the same in the config: left
    /// unread, it would stop applying without a word.
    #[test]
    fn a_folders_own_files_are_errors_that_name_the_entry() {
        for (file, wanted) in [
            (
                "users/[id]/_before.ts",
                "`path = \"/users/{id}/{*rest}\"` and `before = [\"<routes>/users/[id]/guard.ts\"]`",
            ),
            (
                "admin._before.py",
                "`path = \"/admin/{*rest}\"` and `before = [\"<routes>/admin.guard.py\"]`",
            ),
            (
                "_before.ts",
                "`path = \"/{*rest}\"` and `before = [\"<routes>/guard.ts\"]`",
            ),
            ("admin/_policies.toml", "`path = \"/admin/{*rest}\"`"),
        ] {
            let errors = scan(&[file.to_string(), "users/get.ts".to_string()]).unwrap_err();
            assert_eq!(errors.len(), 1, "{errors:?}");
            assert_eq!(errors[0].0, file);
            assert!(errors[0].1.contains(wanted), "{:?}", errors[0]);
        }
    }

    #[test]
    fn errors() {
        let bad: Vec<String> = vec!["files.[...p].x.get.ts".into(), "a/[b-c].get.ts".into()];
        let errors = scan(&bad).unwrap_err();
        assert_eq!(errors.len(), 2);
    }
}
