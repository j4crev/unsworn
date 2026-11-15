//! `--target vercel`: a Vercel deployment in the Build Output API's layout,
//! which `vercel deploy --prebuilt` uploads as it is. Written, by default,
//! to `.vercel/output`:
//!
//! - `config.json`: sends every request to the one function, before
//!   Vercel looks for static files, so routes' policies apply to files too;
//! - `functions/unsworn.func/`: the function (see [`super::functions`]) and
//!   its `.vc-config.json`.
//!
//! Settings Vercel keeps per function (`memory`, `maxDuration`, `regions`,
//! `architecture`, `runtime`) come from `vercel.function.json` in the
//! project, if there is one. A prebuilt deployment doesn't read them from
//! `vercel.json`.

use serde_json::{Map, Value, json};

use super::{Prepared, functions, write};

/// The function's name, and so its own URL path, which nothing routes to.
const FUNCTION: &str = "unsworn";

/// The user's settings for the function.
const USER_CONFIG: &str = "vercel.function.json";

const RUNTIME: &str = "nodejs22.x";

pub fn build(p: Prepared) -> Result<(), String> {
    let dir = p.out.join(format!("functions/{FUNCTION}.func"));
    let entry = functions::entry(
        &p,
        &p.out,
        "import { createVercelFunction } from \"@unswornjs/sdk/vercel\";",
        "createVercelFunction",
    )?;
    let built = functions::build(&p, &dir, &entry)?;

    let path = p.root.join(USER_CONFIG);
    let user = match std::fs::read_to_string(&path) {
        Ok(text) => Some(serde_json::from_str(&text).map_err(|e| format!("{USER_CONFIG}: {e}"))?),
        Err(_) => None,
    };
    let pretty = |v: &Value| serde_json::to_string_pretty(v).unwrap_or_default() + "\n";
    write(
        &dir.join(".vc-config.json"),
        &pretty(&function_config(user)?),
    )?;
    write(&p.out.join("config.json"), &pretty(&output_config()))?;

    p.report(built.assets, &[("deploy:", "npx vercel deploy --prebuilt")]);
    Ok(())
}

/// `.vc-config.json`: the user's settings, plus how Vercel runs the bundle.
fn function_config(user: Option<Value>) -> Result<Value, String> {
    let mut c = match user {
        None => Map::new(),
        Some(Value::Object(m)) => m,
        Some(_) => return Err(format!("{USER_CONFIG} must hold an object")),
    };
    c.entry("runtime").or_insert_with(|| json!(RUNTIME));
    c.insert("handler".into(), json!("index.mjs"));
    c.insert("launcherType".into(), json!("Nodejs"));
    // The entry takes Node's own request and response.
    c.insert("shouldAddHelpers".into(), json!(false));
    c.insert("shouldAddSourcemapSupport".into(), json!(true));
    c.insert("supportsResponseStreaming".into(), json!(true));
    Ok(Value::Object(c))
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
    fn merges_the_user_settings() {
        let user =
            json!({ "memory": 1024, "maxDuration": 60, "regions": ["fra1"], "handler": "mine.js" });
        let c = function_config(Some(user)).unwrap();
        assert_eq!(c["memory"], 1024);
        assert_eq!(c["regions"], json!(["fra1"]));
        assert_eq!(c["handler"], "index.mjs");
        assert_eq!(c["runtime"], RUNTIME);
        assert_eq!(c["supportsResponseStreaming"], true);

        let newer = function_config(Some(json!({ "runtime": "nodejs24.x" }))).unwrap();
        assert_eq!(newer["runtime"], "nodejs24.x");
        assert!(function_config(Some(json!([]))).is_err());

        let routes = &output_config()["routes"];
        assert_eq!(routes[0]["dest"], "/unsworn");
    }
}
