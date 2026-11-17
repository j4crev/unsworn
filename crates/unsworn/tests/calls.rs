//! Handlers calling functions: other handlers, in other languages, through
//! Unsworn.

mod common;

use common::{Project, Server};
use serde_json::{Value, json};

fn config(extra: &str) -> String {
    format!(
        r#"
[server]
response_validation = "enforce"

[runtimes.ts]
extensions = [".ts"]
command = "bun run {root}/sdk/js/src/worker.ts"
protocol = "framed"

[runtimes.py]
extensions = [".py"]
command = "python3 -u -m unsworn.worker"
protocol = "framed"

[auth.key]
type = "api_key"
keys = ["secret"]

# A Python function, called from TypeScript.
[[function]]
name = "count"
handler = "fn/words.py#count"
input = {{ type = "object", required = ["text"], properties = {{ text = {{ type = "string" }} }} }}
output = {{ type = "object", required = ["words"], properties = {{ words = {{ type = "integer" }} }} }}

# A TypeScript function, called from Python.
[[function]]
name = "shout"
handler = "fn/shout.ts#shout"
input = {{ type = "object", required = ["text"] }}

[[function]]
name = "whoami"
handler = "fn/words.py#whoami"

[[function]]
name = "refuse"
handler = "fn/shout.ts#refuse"

[[function]]
name = "sleepy"
handler = "fn/words.py#sleepy"
timeout = "300ms"

[[function]]
name = "lies"
handler = "fn/shout.ts#lies"
output = {{ type = "object", required = ["words"] }}

# Python calling TypeScript calling Python.
[[function]]
name = "relay"
handler = "fn/shout.ts#relay"

[[function]]
name = "loop"
handler = "fn/shout.ts#loop"

[[route]]
path = "/ts/{{what}}"
handler = "src/routes.ts#route"
auth = "key"

[[route]]
path = "/py/{{what}}"
handler = "src/routes.py#route"

[[route]]
path = "/py-async/{{what}}"
handler = "src/routes.py#route_async"
{extra}
"#,
        root = common::repo_root().display()
    )
}

const WORDS_PY: &str = r#"
import time
from unsworn import json

def count(req, ctx):
    return json({"words": len(req.json["text"].split())})

def whoami(req, ctx):
    return json({"auth": ctx.get("auth"), "caller": ctx.get("caller"), "route": req.route})

def sleepy(req, ctx):
    time.sleep(2)
    return json({"woke": True})
"#;

const SHOUT_TS: &str = r#"
export const shout = (req: any) => ({ json: { text: String(req.json.text).toUpperCase() } });
export const refuse = () => ({ status: 409, json: { error: "not now" } });
export const lies = () => ({ json: { nothing: true } });
export const relay = async (req: any, ctx: any) => ({ json: { relayed: await ctx.call("count", req.json) } });
export const loop = async (_req: any, ctx: any) => ({ json: await ctx.call("loop") });
"#;

const ROUTES_TS: &str = r#"
export const route = async (req: any, ctx: any) => {
  const what = req.params.what;
  try {
    if (what === "count") return { json: await ctx.call("count", { text: req.query.q }) };
    if (what === "whoami") return { json: await ctx.call("whoami") };
    if (what === "bad-input") return { json: await ctx.call("count", { text: 5 }) };
    if (what === "missing") return { json: await ctx.call("nope") };
    if (what === "refuse") return { json: await ctx.call("refuse") };
    if (what === "sleepy") return { json: await ctx.call("sleepy") };
    if (what === "lies") return { json: await ctx.call("lies") };
    if (what === "loop") return { json: await ctx.call("loop") };
    if (what === "both") {
      const [a, b] = await Promise.all([ctx.call("count", { text: "a b" }), ctx.call("count", { text: "a b c" })]);
      return { json: { a, b } };
    }
    // The context is still plain data.
    if (what === "ctx") return { json: { keys: Object.keys(ctx), text: JSON.stringify(ctx) } };
  } catch (err: any) {
    return { status: 200, json: { failed: err.status, name: err.name, message: err.message, json: err.json ?? null } };
  }
  return { status: 404 };
};
"#;

const ROUTES_PY: &str = r#"
from unsworn import CallError, json

def route(req, ctx):
    what = req.params["what"]
    try:
        if what == "shout":
            return json(ctx.call("shout", {"text": req.query["q"]}))
        if what == "relay":
            return json(ctx.call("relay", {"text": "one two three"}))
        if what == "bad-input":
            return json(ctx.call("shout", {}))
        if what == "ctx":
            return json({"is_dict": isinstance(ctx, dict), "ctx": ctx})
    except CallError as e:
        return json({"failed": e.status, "message": str(e), "json": e.json})
    return {"status": 404}

async def route_async(req, ctx):
    what = req.params["what"]
    if what == "shout":
        return json(await ctx.call("shout", {"text": req.query["q"]}))
    return {"status": 404}
"#;

fn start(extra: &str) -> Option<(Project, Server)> {
    if !common::has("bun") || !common::has("python3") {
        eprintln!("skipped: needs bun and python3");
        return None;
    }
    let p = Project::new(&[
        ("unsworn.toml", &config(extra)),
        ("fn/words.py", WORDS_PY),
        ("fn/shout.ts", SHOUT_TS),
        ("src/routes.ts", ROUTES_TS),
        ("src/routes.py", ROUTES_PY),
    ]);
    let s = p.serve();
    Some((p, s))
}

fn ts(s: &Server, what: &str) -> Value {
    let r = s.request(
        "GET",
        &format!("/ts/{what}"),
        &[("x-api-key", "secret")],
        b"",
    );
    assert_eq!(r.status, 200, "{}\n{}", r.text(), s.logs());
    r.json()
}

fn py(s: &Server, what: &str) -> Value {
    let r = s.get(&format!("/py/{what}"));
    assert_eq!(r.status, 200, "{}\n{}", r.text(), s.logs());
    r.json()
}

#[test]
fn typescript_calls_python_and_python_calls_typescript() {
    let Some((_p, s)) = start("") else { return };
    assert_eq!(ts(&s, "count?q=one%20two%20three"), json!({ "words": 3 }));
    assert_eq!(py(&s, "shout?q=hello"), json!({ "text": "HELLO" }));
    // From an `async def` handler too.
    let r = s.get("/py-async/shout?q=hi");
    assert_eq!(r.json(), json!({ "text": "HI" }), "{}", s.logs());
    // Two at once, and a function that calls a function.
    assert_eq!(
        ts(&s, "both"),
        json!({ "a": { "words": 2 }, "b": { "words": 3 } })
    );
    assert_eq!(py(&s, "relay"), json!({ "relayed": { "words": 3 } }));
}

#[test]
fn a_function_gets_the_callers_context_and_says_who_called() {
    let Some((_p, s)) = start("") else { return };
    let who = ts(&s, "whoami");
    // The caller's verified credentials, which the caller didn't send.
    assert_eq!(who["auth"]["scheme"], "key", "{who}");
    assert_eq!(
        who["caller"],
        json!({ "function": "whoami", "from": "src/routes.ts#route", "depth": 1 })
    );
    assert_eq!(who["route"], "fn:whoami");
    // `call` isn't one of the context's keys in either language.
    let ctx = ts(&s, "ctx");
    assert_eq!(ctx["keys"], json!(["auth"]), "{ctx}");
    assert!(!ctx["text"].as_str().unwrap().contains("call"), "{ctx}");
    assert_eq!(py(&s, "ctx"), json!({ "is_dict": true, "ctx": {} }));
}

#[test]
fn failures_reach_the_caller_as_errors_with_a_status() {
    let Some((_p, s)) = start("") else { return };
    // Input the function's schema refuses never reaches the function.
    let bad = ts(&s, "bad-input");
    assert_eq!(bad["failed"], 422, "{bad}");
    assert_eq!(bad["name"], "CallError");
    assert_eq!(bad["json"]["errors"][0]["location"], "input.text", "{bad}");
    let bad = py(&s, "bad-input");
    assert_eq!(bad["failed"], 422, "{bad}");

    let missing = ts(&s, "missing");
    assert_eq!(missing["failed"], 404, "{missing}");
    assert!(
        missing["message"]
            .as_str()
            .unwrap()
            .contains("no function named \"nope\""),
        "{missing}"
    );

    // A function's own error status, with what it answered.
    let refused = ts(&s, "refuse");
    assert_eq!(refused["failed"], 409);
    assert_eq!(refused["json"], json!({ "error": "not now" }));

    // Its own timeout, shorter than the route's.
    let began = std::time::Instant::now();
    let slow = ts(&s, "sleepy");
    assert_eq!(slow["failed"], 504, "{slow}");
    assert!(
        began.elapsed() < std::time::Duration::from_millis(1500),
        "{:?}",
        began.elapsed()
    );

    // Output that isn't what the function's schema says.
    let lies = ts(&s, "lies");
    assert_eq!(lies["failed"], 502, "{lies}");

    // A function that calls itself stops, with a reason. The innermost
    // call gets a 508; each function above it fails for not handling
    // that, which is a 500 carrying the reason.
    let looped = ts(&s, "loop");
    assert_eq!(looped["failed"], 500, "{looped}");
    assert!(
        looped["message"]
            .as_str()
            .unwrap()
            .contains("nested more than 16 deep"),
        "{looped}"
    );
}

#[test]
fn functions_are_checked_and_planned() {
    let p = Project::new(&[
        ("unsworn.toml", &config("")),
        ("fn/words.py", ""),
        ("fn/shout.ts", ""),
        ("src/routes.ts", ""),
        ("src/routes.py", ""),
    ]);
    let out = p.command(&["plan", "-o", "-"]);
    let plan = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        plan.contains("\nfunction sleepy\n  handler            fn/words.py#sleepy (py)\n  timeout            300ms\n"),
        "{plan}"
    );
    assert!(
        plan.contains("\nfunction count\n  handler            fn/words.py#count (py)\n  input "),
        "{plan}"
    );

    // A function that can't run on a target is reported for it.
    let out = p.command(&["check", "--target", "cloudflare"]);
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(err.contains("only run on the server"), "{err}");

    let bad = config("\n[[function]]\nname = \"count\"\nhandler = \"fn/missing.py\"\n");
    p.write("unsworn.toml", &bad);
    let out = p.command(&["check"]);
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        err.contains("there is already a function named \"count\""),
        "{err}"
    );
}
