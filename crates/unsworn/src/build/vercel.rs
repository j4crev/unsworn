//! `--target vercel`: a Vercel deployment in the Build Output API's layout,
//! which `vercel deploy --prebuilt` uploads as it is. Written, by default,
//! to `.vercel/output`:
//!
//! - `config.json`: sends every request to the one function, before
//!   Vercel looks for static files, so routes' policies apply to files too;
//! - `functions/unsworn.func/`: the function (see [`super::functions`]) and
//!   its `.vc-config.json`.
//!
//! Vercel's own build is what applies `functions` and `regions` from
//! `vercel.json`, and a prebuilt deployment skips it. So this build reads
//! them: `regions`, and `memory` and `maxDuration` from the entry in
//! `functions`, go into the function's `.vc-config.json`. The entry's key
//! doesn't matter, since there's one function.

use serde_json::{Map, Value, json};

use super::{Prepared, functions, write};

/// The function's name, and so its own URL path, which nothing routes to.
const FUNCTION: &str = "unsworn";

/// The user's Vercel config.
const USER_CONFIG: &str = "vercel.json";

/// What a `functions` entry may set here. The rest of what Vercel allows
/// there (`runtime`, `includeFiles`, ...) is about building from source.
const FUNCTION_SETTINGS: &[&str] = &["memory", "maxDuration"];

const RUNTIME: &str = "nodejs22.x";

pub fn build(p: Prepared) -> Result<(), String> {
    let dir = p.out.join(format!("functions/{FUNCTION}.func"));
    let entry = functions::entry(
        &p,
        &p.out,
        "import { createVercelFunction } from \"@unswornjs/sdk/vercel\";",
        "export default createVercelFunction",
    )?;
    let built = functions::build(&p, &dir, &entry)?;

    let path = p.root.join(USER_CONFIG);
    let user: Option<Value> = match std::fs::read_to_string(&path) {
        Ok(text) => Some(serde_json::from_str(&text).map_err(|e| format!("{USER_CONFIG}: {e}"))?),
        Err(_) => None,
    };
    let (config, ignored) = function_config(user.as_ref())?;
    for key in ignored {
        eprintln!(
            "! {USER_CONFIG}: `{key}` in `functions` is ignored; the build sets how the function is bundled and run"
        );
    }
    let pretty = |v: &Value| serde_json::to_string_pretty(v).unwrap_or_default() + "\n";
    write(&dir.join(".vc-config.json"), &pretty(&config))?;
    write(&p.out.join("config.json"), &pretty(&output_config()))?;

    p.report(built.assets, &[("deploy:", "npx vercel deploy --prebuilt")]);
    Ok(())
}

/// `.vc-config.json`: the settings from the user's `vercel.json`, plus how
/// Vercel runs the bundle. Also returns the keys of the `functions` entry
/// that don't apply.
fn function_config(user: Option<&Value>) -> Result<(Value, Vec<String>), String> {
    let mut c = Map::new();
    let mut ignored = Vec::new();
    if let Some(user) = user {
        let Some(user) = user.as_object() else {
            return Err(format!("{USER_CONFIG} must hold an object"));
        };
        if let Some(regions) = user.get("regions") {
            c.insert("regions".into(), regions.clone());
        }
        match user.get("functions") {
            None => {}
            Some(Value::Object(entries)) if entries.len() <= 1 => {
                for (pattern, settings) in entries {
                    let Some(settings) = settings.as_object() else {
                        return Err(format!(
                            "{USER_CONFIG}: `functions.{pattern}` must hold an object"
                        ));
                    };
                    for (key, value) in settings {
                        if FUNCTION_SETTINGS.contains(&key.as_str()) {
                            c.insert(key.clone(), value.clone());
                        } else {
                            ignored.push(key.clone());
                        }
                    }
                }
            }
            Some(Value::Object(_)) => {
                return Err(format!(
                    "{USER_CONFIG}: `functions` has several entries, but the project is one function; keep one"
                ));
            }
            Some(_) => return Err(format!("{USER_CONFIG}: `functions` must hold an object")),
        }
    }
    c.insert("runtime".into(), json!(RUNTIME));
    c.insert("handler".into(), json!("index.mjs"));
    c.insert("launcherType".into(), json!("Nodejs"));
    // The entry takes Node's own request and response.
    c.insert("shouldAddHelpers".into(), json!(false));
    c.insert("shouldAddSourcemapSupport".into(), json!(true));
    c.insert("supportsResponseStreaming".into(), json!(true));
    Ok((Value::Object(c), ignored))
}

/// `config.json`: every path goes to the function. The rule comes before
/// the filesystem phase, and there are no static files to find anyway.
fn output_config() -> Value {
    json!({
        "version": 3,
        "routes": [{ "src": "/(.*)", "dest": format!("/{FUNCTION}") }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takes_function_settings_from_vercel_json() {
        let user = json!({
            "regions": ["fra1"],
            "rewrites": [],
            "functions": { "api/*.ts": { "memory": 1024, "maxDuration": 60, "runtime": "vercel-php@0.9.0" } },
        });
        let (c, ignored) = function_config(Some(&user)).unwrap();
        assert_eq!(c["memory"], 1024);
        assert_eq!(c["maxDuration"], 60);
        assert_eq!(c["regions"], json!(["fra1"]));
        assert_eq!(c["handler"], "index.mjs");
        assert_eq!(c["runtime"], RUNTIME);
        assert_eq!(c["supportsResponseStreaming"], true);
        assert!(c.get("rewrites").is_none());
        assert_eq!(ignored, ["runtime"]);

        let (bare, ignored) = function_config(None).unwrap();
        assert!(bare.get("memory").is_none() && ignored.is_empty());
        let two = json!({ "functions": { "a": {}, "b": {} } });
        assert!(
            function_config(Some(&two))
                .unwrap_err()
                .contains("one function")
        );
        assert!(function_config(Some(&json!([]))).is_err());

        let routes = &output_config()["routes"];
        assert_eq!(routes[0]["dest"], "/unsworn");
    }
}
