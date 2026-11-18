//! What `unsworn build` puts in a bundle for an edge or serverless target:
//! the project's config documents as JSON, and an error where one would
//! read differently that way.

mod common;

use common::Project;
use serde_json::Value;

fn stderr(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

const YAML: &str = r#"runtimes:
  ts:
    extensions: [.ts]
    command: bun run worker.ts
    env:
      PORT: 3000
route:
  - path: /users/{id}
    method: GET
    handler: src/users.ts#get
"#;

#[test]
fn yaml_that_json_reads_differently_is_an_error_for_a_built_target() {
    let p = Project::new(&[("unsworn.yaml", YAML), ("src/users.ts", "")]);
    let config = ["--config", "unsworn.yaml"];

    // The server reads the YAML itself, and it's fine there.
    let out = p.command(&[&config[..], &["check"]].concat());
    assert!(out.status.success(), "{}", stderr(&out));

    for target in ["cloudflare", "vercel", "netlify", "lambda"] {
        let out = p.command(&[&config[..], &["check", "--target", target]].concat());
        assert!(!out.status.success(), "{target} should be refused");
        let text = stderr(&out);
        assert!(
            text.contains(
                "✗ unsworn.yaml runtimes.ts.env.PORT: 3000 is a number, and this setting is text\n  → write it in quotes: \"3000\"."
            ),
            "{target}: {text}"
        );
    }
    // The build stops before it looks for the SDK or writes anything.
    let out = p.command(&[&config[..], &["build", "--target", "cloudflare"]].concat());
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("runtimes.ts.env.PORT"),
        "{}",
        stderr(&out)
    );
    assert!(!p.path().join("dist").exists());

    p.write(
        "unsworn.yaml",
        &YAML.replace("PORT: 3000", "PORT: \"3000\""),
    );
    let out = p.command(&[&config[..], &["check", "--target", "cloudflare"]].concat());
    assert!(out.status.success(), "{}", stderr(&out));
}

#[test]
fn a_bundle_holds_config_documents_as_json() {
    let config = r#"
[runtimes.ts]
extensions = [".ts"]
command = "bun run worker.ts"

[routes]
dir = "routes"

[auth.jwt]
type = "jwt"
public_key = "keys/jwt.pem"

[openapi]
spec = "openapi.yaml"
handler = "src/{operationId}.ts"

[[route]]
path = "/users"
method = "POST"
handler = "src/users.ts#create"
body = "schemas/user.yaml"
auth = "jwt"

[[route]]
path = "/up/{*rest}"
proxy = "{env.BUNDLE_TEST_UPSTREAM_THAT_IS_NOT_SET}"
"#;
    let spec = "openapi: 3.0.3\ninfo: { title: T, version: 1.0.0 }\npaths:\n  /pets:\n    get:\n      operationId: listPets\n      responses:\n        200: { description: ok }\n";
    let pem = "-----BEGIN PUBLIC KEY-----\nabc\n-----END PUBLIC KEY-----\n";
    let p = Project::new(&[
        ("unsworn.toml", config),
        ("keys/jwt.pem", pem),
        ("openapi.yaml", spec),
        (
            "schemas/user.yaml",
            "type: object\nrequired: [name]\nproperties:\n  name: { type: string }\n",
        ),
        (
            "routes/_policies.toml",
            "rate_limit = { requests = 60, per = \"1m\" }\n",
        ),
        ("routes/api.items.get.ts", ""),
        ("src/users.ts", ""),
        ("src/listPets.ts", ""),
    ]);
    let out = p.command(&["bundle-files"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let bundle: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(bundle["config"], "unsworn.toml");
    let files = &bundle["files"];

    let document = |path: &str| -> Value {
        let text = files[path]
            .as_str()
            .unwrap_or_else(|| panic!("{path} has no text"));
        serde_json::from_str(text).unwrap_or_else(|e| panic!("{path} isn't JSON ({e}): {text}"))
    };
    // A variable stays for the platform to supply.
    assert_eq!(
        document("unsworn.toml")["route"][1]["proxy"],
        "{env.BUNDLE_TEST_UPSTREAM_THAT_IS_NOT_SET}"
    );
    assert_eq!(
        document("openapi.yaml")["paths"]["/pets"]["get"]["responses"]["200"]["description"],
        "ok"
    );
    assert_eq!(document("schemas/user.yaml")["required"][0], "name");
    assert_eq!(
        document("routes/_policies.toml")["rate_limit"]["requests"],
        60
    );
    // Files that aren't config are as they were: a key's text, and only
    // the names of handler modules.
    assert_eq!(files["keys/jwt.pem"], pem);
    assert_eq!(files["src/users.ts"], Value::Null);
    assert_eq!(files["routes/api.items.get.ts"], Value::Null);
}
