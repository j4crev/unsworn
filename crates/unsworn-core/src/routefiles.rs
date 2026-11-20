//! File-based routes: nested or flat.
//!
//! A route is built from the directory names followed by the parts of the
//! file name split on `.`, where the last part before the extension is the
//! HTTP method. `[name]` is a parameter, `[...name]` matches the rest of the
//! path, `(name)` is a group that doesn't appear in the URL, and `[.]` is a
//! literal dot. A `_before.<ext>` file is a `before` handler for every
//! route file in its folder and below.

pub const METHODS: &[&str] = &["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"];

/// A route defined by a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRoute {
    pub method: String,
    /// Route pattern, e.g. `/users/{id}`.
    pub path: String,
    /// Path of the file relative to the routes directory.
    pub file: String,
    /// The logical folder chain: directories plus file-name parts before the
    /// method, as written (`["(admin)", "users", "[id]"]`).
    pub chain: Vec<String>,
}

/// A `_before.<ext>` file and the folder it covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedFile {
    pub file: String,
    pub scope: Vec<String>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Scan {
    pub routes: Vec<FileRoute>,
    pub befores: Vec<ScopedFile>,
}

/// Classifies files from a routes directory. Paths are relative to the
/// directory and use `/`. Files that aren't routes, such as shared helper
/// modules, are ignored.
pub fn scan(files: &[String]) -> Result<Scan, Vec<(String, String)>> {
    let mut out = Scan::default();
    let mut errors = Vec::new();
    for file in files {
        match classify(file) {
            Ok(Some(Item::Route(r))) => out.routes.push(r),
            Ok(Some(Item::Before(s))) => out.befores.push(s),
            Ok(None) => {}
            Err(e) => errors.push((file.clone(), e)),
        }
    }
    out.routes
        .sort_by(|a, b| a.path.cmp(&b.path).then(a.method.cmp(&b.method)));
    // Outermost scopes first, so they merge first.
    out.befores.sort_by_key(|s| s.scope.len());
    if errors.is_empty() {
        Ok(out)
    } else {
        Err(errors)
    }
}

enum Item {
    Route(FileRoute),
    Before(ScopedFile),
}

fn classify(file: &str) -> Result<Option<Item>, String> {
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
        return Ok(Some(Item::Before(ScopedFile {
            file: file.to_string(),
            scope: chain,
        })));
    }
    let method = last.to_ascii_uppercase();
    if !METHODS.contains(&method.as_str()) || last.chars().any(|c| c.is_ascii_uppercase()) {
        return Ok(None);
    }
    let path = chain_to_path(&chain)?;
    Ok(Some(Item::Route(FileRoute {
        method,
        path,
        file: file.to_string(),
        chain,
    })))
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

/// True if `scope` is a prefix of `chain`.
pub fn in_scope(scope: &[String], chain: &[String]) -> bool {
    scope.len() <= chain.len() && scope.iter().zip(chain).all(|(a, b)| a == b)
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
    fn helpers_are_ignored_and_scoped_files_found() {
        let files: Vec<String> = [
            "users/db.ts",
            "users/_before.ts",
            "admin/stats.get.ts",
            ".hidden.get.ts",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let scan = scan(&files).unwrap();
        assert_eq!(scan.routes.len(), 1);
        assert_eq!(scan.befores[0].scope, vec!["users"]);
        assert!(!in_scope(&scan.befores[0].scope, &scan.routes[0].chain));
    }

    #[test]
    fn errors() {
        let bad: Vec<String> = vec!["files.[...p].x.get.ts".into(), "a/[b-c].get.ts".into()];
        let errors = scan(&bad).unwrap_err();
        assert_eq!(errors.len(), 2);
    }
}
