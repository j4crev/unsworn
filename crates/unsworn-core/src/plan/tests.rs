use super::*;
use crate::project::compile_symbolic;
use crate::project::tests::MemHost;

const BASE: &str = r#"
[runtimes.ts]
extensions = [".ts"]
command = "bun run worker.ts"

[auth.jwt]
type = "jwt"
url = "{env.AUTH_URL}/jwks.json"
audience = "api"

[auth.key]
type = "api_key"
keys = "{env.API_KEYS}"
"#;

fn plan_of(config: &str, files: &[(&str, &str)]) -> Plan {
    // A project with auth schemes has every route say who may call it.
    // Here that's anyone, unless the test has a `[defaults]` of its own.
    let defaults = match config.contains("[defaults]") {
        true => "",
        false => "[defaults]\nauth = \"none\"\n",
    };
    let text = format!("{BASE}\n{defaults}\n{config}");
    let mut all = vec![("unsworn.toml", text.as_str())];
    all.extend_from_slice(files);
    let host = MemHost::with(&all);
    let compiled = compile_symbolic(&host, "unsworn.toml");
    let errors: Vec<String> = compiled
        .diagnostics
        .iter()
        .filter(|d| d.is_error())
        .map(ToString::to_string)
        .collect();
    assert!(errors.is_empty(), "{errors:?}");
    Plan::of(&compiled.project.expect("a project"))
}

fn entry<'a>(plan: &'a Plan, title: &str) -> &'a Entry {
    plan.entries
        .iter()
        .find(|e| e.title() == title)
        .unwrap_or_else(|| {
            let titles: Vec<String> = plan.entries.iter().map(Entry::title).collect();
            panic!("no entry {title:?} among {titles:?}")
        })
}

fn text(e: &Entry, key: &str) -> String {
    e.fact(key)
        .unwrap_or_else(|| panic!("no fact {key:?} in {:?}", e.facts))
        .text
        .clone()
}

const APP: &str = r#"
[defaults]
auth = "jwt"

[[route]]
path = "/{*rest}"
proxy = "{env.APP_URL}"

[[route]]
path = "/assets/{*rest}"
auth = "none"

[[route]]
path = "/api/{*rest}"
rate_limit = { requests = 100, per = "1m", key = "auth.sub" }

[[route]]
path = "/api/users/{id}"
method = ["GET", "DELETE"]
handler = "src/users.ts#user"
params = { id = { type = "integer", min = 1 } }
"#;

#[test]
fn needs_no_environment_and_writes_no_secrets() {
    // `compile` would refuse this config: none of its variables are set.
    let plan = plan_of(APP, &[("src/users.ts", "")]);
    let jwt = entry(&plan, "auth jwt");
    assert_eq!(text(jwt, "keys"), "from {env.AUTH_URL}/jwks.json");
    assert_eq!(text(jwt, "audience"), "api");
    assert_eq!(
        text(entry(&plan, "auth key"), "keys"),
        "listed in the config"
    );
    assert_eq!(text(entry(&plan, "* /{*rest}"), "proxy"), "{env.APP_URL}");
    assert!(!plan.render().contains("API_KEYS"));
}

#[test]
fn a_route_has_what_every_layer_adds_up_to() {
    let plan = plan_of(APP, &[("src/users.ts", "")]);
    let get = entry(&plan, "GET /api/users/{id}");
    assert_eq!(text(get, "handler"), "src/users.ts#user (ts)");
    assert_eq!(text(get, "auth"), "jwt");
    assert_eq!(
        text(get, "rate_limit"),
        "100 per 1m by auth.sub, counted for * /api/{*rest}"
    );
    assert_eq!(text(get, "params.id"), r#"{"minimum":1,"type":"integer"}"#);
    assert!(get.inside.is_empty());
}

#[test]
fn a_catch_all_lists_the_paths_inside_it_with_other_rules() {
    let plan = plan_of(APP, &[("src/users.ts", "")]);
    let all = entry(&plan, "* /{*rest}");
    assert_eq!(text(all, "auth"), "jwt");
    assert!(all.fact("rate_limit").is_none());
    let titles: Vec<String> = all.inside.iter().map(Entry::title).collect();
    assert_eq!(titles, ["at /api/{*rest}", "at /assets/{*rest}"]);
    assert_eq!(
        text(&all.inside[0], "rate_limit"),
        "100 per 1m by auth.sub, counted for * /api/{*rest}"
    );
    assert_eq!(text(&all.inside[1], "auth"), "none");
}

#[test]
fn renders_methods_that_agree_as_one_block() {
    let plan = plan_of(APP, &[("src/users.ts", "")]);
    let out = plan.render();
    assert!(out.contains("\nplan 1\n"), "{out}");
    assert!(
        out.contains(
            "\nGET, DELETE /api/users/{id}\n  handler            src/users.ts#user (ts)\n  auth               jwt\n  rate_limit         100 per 1m by auth.sub, counted for * /api/{*rest}\n  params.id          {\"minimum\":1,\"type\":\"integer\"}\n"
        ),
        "{out}"
    );
    // Inside the catch-all, only what differs from it.
    assert!(
        out.contains(
            "\n* /{*rest}\n  proxy              {env.APP_URL}\n  auth               jwt\n  at /api/{*rest}\n    rate_limit         100 per 1m by auth.sub, counted for * /api/{*rest}\n  at /assets/{*rest}\n    auth               none\n"
        ),
        "{out}"
    );
    // The server's own timeout and body limit aren't repeated on each route.
    assert_eq!(out.matches("max_body").count(), 1, "{out}");
    // Rendering is stable.
    assert_eq!(out, plan_of(APP, &[("src/users.ts", "")]).render());
}

#[test]
fn an_entry_for_some_methods_shows_on_a_route_for_all() {
    let plan = plan_of(
        r#"
[[route]]
path = "/{*rest}"
proxy = "http://app"

[[route]]
path = "/{*rest}"
method = "POST"
auth = "jwt"
"#,
        &[],
    );
    let all = entry(&plan, "* /{*rest}");
    assert_eq!(text(all, "auth"), "none");
    assert_eq!(all.inside.len(), 1);
    assert_eq!(all.inside[0].title(), "at POST /{*rest}");
    assert_eq!(text(&all.inside[0], "auth"), "jwt");
}

#[test]
fn switching_a_policy_off_inside_a_route_is_written() {
    let plan = plan_of(
        r#"
[defaults]
auth = "none"
rate_limit = { requests = 5, per = "1s", key = "ip" }

[[route]]
path = "/{*rest}"
proxy = "http://app"

[[route]]
path = "/health/{*rest}"
rate_limit = "off"
"#,
        &[],
    );
    let out = plan.render();
    assert!(
        out.contains("  at /health/{*rest}\n    rate_limit         off\n"),
        "{out}"
    );
}

fn changes(old: &str, new: &str) -> Diff {
    let files = [("src/users.ts", ""), ("src/gate.ts", "")];
    diff(&plan_of(old, &files), &plan_of(new, &files))
}

fn only_change(d: &Diff) -> (&str, &Change) {
    assert_eq!(d.items.len(), 1, "{d:?}");
    match &d.items[0] {
        (title, Delta::Changed(list)) => {
            assert_eq!(list.len(), 1, "{list:?}");
            (title.as_str(), &list[0])
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_same_rules_written_differently_dont_differ() {
    // A policy set instead of the route's own keys, and another order.
    let a = r#"
[[route]]
path = "/b"
handler = "src/users.ts"
auth = "jwt"
timeout = "5s"

[[route]]
path = "/a"
respond = { status = 204 }
"#;
    let b = r#"
[policies.private]
auth = "jwt"
timeout = "5s"

[[route]]
path = "/a"
respond = { status = 204 }

[[route]]
path = "/b"
handler = "src/users.ts"
use = ["private"]
"#;
    let d = changes(a, b);
    assert!(d.is_empty(), "{d:?}");
    assert_eq!(d.render(), "The rules are the same.\n");
}

#[test]
fn removing_auth_is_looser() {
    let old = "[[route]]\npath = \"/admin/{*rest}\"\nproxy = \"http://app\"\nauth = \"jwt\"\n";
    let new = "[[route]]\npath = \"/admin/{*rest}\"\nproxy = \"http://app\"\n";
    let d = changes(old, new);
    let (title, c) = only_change(&d);
    assert_eq!(title, "* /admin/{*rest}");
    assert_eq!(
        (c.key.as_str(), c.old.as_deref(), c.new.as_deref()),
        ("auth", Some("jwt"), Some("none"))
    );
    assert_eq!(c.direction, Direction::Looser);
    assert_eq!(d.looser(), 1);
    let out = d.render();
    assert!(
        out.contains("~ * /admin/{*rest}\n  ! auth               jwt → none  (looser)\n"),
        "{out}"
    );
    assert!(out.ends_with("\n1 change (1 looser)\n"), "{out}");

    let d = changes(new, old);
    assert_eq!(only_change(&d).1.direction, Direction::Tighter);
}

#[test]
fn auth_directions() {
    let route = |auth: &str| {
        format!("[[route]]\npath = \"/x\"\nhandler = \"src/users.ts\"\nauth = {auth}\n")
    };
    let dir = |old: &str, new: &str| {
        let d = changes(&route(old), &route(new));
        only_change(&d).1.direction
    };
    // Fewer scopes asked for, or another way in.
    let scoped = r#"{ scheme = "jwt", scopes = ["a", "b"] }"#;
    let one = r#"{ scheme = "jwt", scopes = ["a"] }"#;
    assert_eq!(dir(scoped, one), Direction::Looser);
    assert_eq!(dir(one, scoped), Direction::Tighter);
    assert_eq!(dir("\"jwt\"", "[\"jwt\", \"key\"]"), Direction::Looser);
    assert_eq!(dir("[\"jwt\", \"key\"]", "\"key\""), Direction::Tighter);
    // Another scheme is neither.
    assert_eq!(dir("\"jwt\"", "\"key\""), Direction::Changed);
}

#[test]
fn limits_and_checks_have_directions() {
    let route = |extra: &str| {
        format!(
            "[[route]]\npath = \"/x\"\nmethod = \"POST\"\nhandler = \"src/users.ts\"\n{extra}\n"
        )
    };
    let dir = |old: &str, new: &str| {
        let d = changes(&route(old), &route(new));
        let (_, c) = only_change(&d);
        (c.key.clone(), c.direction)
    };
    let limit = |n: u32| format!("rate_limit = {{ requests = {n}, per = \"1m\", key = \"ip\" }}");
    assert_eq!(
        dir(&limit(10), &limit(100)),
        ("rate_limit".into(), Direction::Looser)
    );
    assert_eq!(
        dir(
            &limit(10),
            "rate_limit = { requests = 1, per = \"1s\", key = \"ip\" }"
        ),
        ("rate_limit".into(), Direction::Looser)
    );
    assert_eq!(
        dir(&limit(10), ""),
        ("rate_limit".into(), Direction::Looser)
    );
    assert_eq!(
        dir("", &limit(10)),
        ("rate_limit".into(), Direction::Tighter)
    );
    assert_eq!(
        dir(
            &limit(10),
            "rate_limit = { requests = 10, per = \"1m\", key = \"auth.sub\" }"
        ),
        ("rate_limit".into(), Direction::Changed)
    );
    assert_eq!(
        dir("max_body = \"1KB\"", "max_body = \"1GB\""),
        ("max_body".into(), Direction::Looser)
    );
    assert_eq!(
        dir("body = { type = \"object\" }", ""),
        ("body".into(), Direction::Looser)
    );
    assert_eq!(
        dir(
            "body = { type = \"object\" }",
            "body = { type = \"object\", required = [\"a\"] }"
        ),
        ("body".into(), Direction::Changed)
    );
    assert_eq!(
        dir("before = [\"src/gate.ts\"]", ""),
        ("before".into(), Direction::Looser)
    );
    assert_eq!(
        dir("", "cors = { origins = \"*\" }"),
        ("cors".into(), Direction::Looser)
    );
    assert_eq!(
        dir(
            "cors = { origins = \"*\" }",
            "cors = { origins = [\"https://a.example\"] }"
        ),
        ("cors".into(), Direction::Tighter)
    );
    assert_eq!(
        dir(
            "origin = { allow = [\"https://a.example\"] }",
            "origin = { allow = [\"https://a.example\", \"https://b.example\"] }"
        ),
        ("origin".into(), Direction::Looser)
    );
    assert_eq!(
        dir("timeout = \"5s\"", "timeout = \"10s\""),
        ("timeout".into(), Direction::Changed)
    );
}

#[test]
fn a_change_inside_a_catch_all_is_reported_there() {
    // `/assets` was behind the app's auth, and now it isn't.
    let old =
        "[defaults]\nauth = \"jwt\"\n\n[[route]]\npath = \"/{*rest}\"\nproxy = \"http://app\"\n";
    let new = format!("{old}\n[[route]]\npath = \"/assets/{{*rest}}\"\nauth = \"none\"\n");
    let d = changes(old, &new);
    let (title, c) = only_change(&d);
    assert_eq!(title, "* /{*rest}");
    assert_eq!(c.at.as_deref(), Some("at /assets/{*rest}"));
    assert_eq!(
        (c.old.as_deref(), c.new.as_deref(), c.direction),
        (Some("jwt"), Some("none"), Direction::Looser)
    );
    let out = d.render();
    assert!(
        out.contains(
            "~ * /{*rest}\n    at /assets/{*rest}\n  !   auth               jwt → none  (looser)\n"
        ),
        "{out}"
    );
}

#[test]
fn a_part_of_a_route_doesnt_repeat_what_it_inherits() {
    let app = |auth: &str| {
        format!(
            "[[route]]\npath = \"/{{*rest}}\"\nproxy = \"http://app\"\nauth = \"{auth}\"\n\n[[route]]\npath = \"/api/{{*rest}}\"\ntimeout = \"5s\"\n"
        )
    };
    let d = changes(&app("jwt"), &app("key"));
    let (_, c) = only_change(&d);
    assert_eq!(c.at, None);
    assert_eq!(c.key, "auth");
}

#[test]
fn added_and_removed_routes() {
    let old = "[[route]]\npath = \"/old\"\nmethod = \"GET\"\nrespond = { status = 204 }\n";
    let new = "[[route]]\npath = \"/new\"\nmethod = [\"GET\", \"POST\"]\nhandler = \"src/users.ts\"\nauth = \"jwt\"\n";
    let d = changes(old, new);
    let out = d.render();
    assert!(
        out.contains("+ GET, POST /new\n    handler            src/users.ts (ts)\n    auth               jwt\n"),
        "{out}"
    );
    assert!(out.contains("- GET /old\n"), "{out}");
    assert!(out.ends_with("\n1 added, 1 removed\n"), "{out}");
    assert_eq!(d.looser(), 0);

    // A new route anyone may call lets through requests that nothing did
    // before, so it counts as looser, on all of its paths or some.
    let public = "[[route]]\npath = \"/new\"\nrespond = { status = 204 }\n";
    let d = changes(old, public);
    assert_eq!((d.looser(), d.opened()), (1, 1));
    let out = d.render();
    assert!(
        out.contains("+ * /new\n    respond            204\n  ! auth               none  (looser: anyone may call it)\n"),
        "{out}"
    );
    assert!(
        out.ends_with("\n1 added (1 open to anyone), 1 removed\n"),
        "{out}"
    );
    let partly = "[[route]]\npath = \"/app/{*rest}\"\nproxy = \"http://app\"\nauth = \"jwt\"\n\n[[route]]\npath = \"/app/assets/{*rest}\"\nauth = \"none\"\n";
    let d = changes(old, partly);
    assert_eq!(d.opened(), 1);
    let out = d.render();
    assert!(
        out.contains("    at /app/assets/{*rest}\n  !   auth               none  (looser: anyone may call it)\n"),
        "{out}"
    );
}

#[test]
fn the_servers_default_is_reported_once() {
    let app = |max: &str| {
        format!(
            "[server]\nmax_body = \"{max}\"\n\n[[route]]\npath = \"/a\"\nrespond = {{ status = 204 }}\n\n[[route]]\npath = \"/b\"\nrespond = {{ status = 204 }}\n"
        )
    };
    let d = changes(&app("1MB"), &app("1GB"));
    let (title, c) = only_change(&d);
    assert_eq!(title, "server");
    assert_eq!(
        (c.key.as_str(), c.direction),
        ("max_body", Direction::Looser)
    );
}

#[test]
fn a_long_schema_is_spread_over_lines() {
    let body = |extra: &str| {
        format!(
            "[[route]]\npath = \"/x\"\nmethod = \"POST\"\nhandler = \"src/users.ts\"\nbody = \"schema.json\"\n# {extra}\n"
        )
    };
    let schema = |max: u32| {
        format!(
            r#"{{"type":"object","required":["name","email"],"properties":{{"name":{{"type":"string","maxLength":{max}}},"email":{{"type":"string","format":"email"}}}}}}"#
        )
    };
    let files = |max: u32| {
        vec![
            ("src/users.ts", String::new()),
            ("schema.json", schema(max)),
        ]
    };
    let plan = |max: u32| {
        let files = files(max);
        let refs: Vec<(&str, &str)> = files.iter().map(|(k, v)| (*k, v.as_str())).collect();
        plan_of(&body(""), &refs)
    };
    let out = plan(10).render();
    assert!(out.contains("  body               {\n"), "{out}");
    assert!(out.contains("\"maxLength\": 10"), "{out}");
    let d = diff(&plan(10), &plan(99));
    let out = d.render();
    assert!(out.contains("-"), "{out}");
    assert!(out.contains("\"maxLength\": 99"), "{out}");
}

#[test]
fn mcp_tools_are_entries() {
    let plan = plan_of(
        r#"
[mcp]
auth = "jwt"

[[mcp.tool]]
name = "create_invoice"
description = "Create a draft invoice"
input = { type = "object" }
handler = "src/users.ts#invoice"
auth = { scheme = "jwt", scopes = ["billing:write"] }
"#,
        &[("src/users.ts", "")],
    );
    let tool = entry(&plan, "tool create_invoice");
    assert_eq!(text(tool, "description"), "Create a draft invoice");
    assert_eq!(text(tool, "handler"), "src/users.ts#invoice (ts)");
    assert_eq!(text(tool, "auth"), "jwt, then jwt [billing:write]");
    assert_eq!(text(tool, "input"), r#"{"type":"object"}"#);
    let endpoint = entry(&plan, "POST /mcp");
    assert_eq!(text(endpoint, "mcp"), "tools run as handlers");
    assert_eq!(text(endpoint, "auth"), "jwt");
}

#[test]
fn patterns_intersect() {
    let i = |a: &str, b: &str| intersect(&parse_pattern(a), &parse_pattern(b)).map(|s| pattern(&s));
    assert_eq!(i("/{*rest}", "/api/{*r}"), Some("/api/{*r}".into()));
    assert_eq!(i("/api/{*rest}", "/api/x"), Some("/api/x".into()));
    assert_eq!(i("/api/{*rest}", "/api"), Some("/api".into()));
    assert_eq!(i("/users/{id}", "/users/me/{*r}"), Some("/users/me".into()));
    assert_eq!(
        i("/users/{id}", "/users/{x}/{*r}"),
        Some("/users/{id}".into())
    );
    assert_eq!(i("/users/{id}", "/orders/{*r}"), None);
    assert_eq!(i("/a", "/a/b/{*r}"), None);
    assert_eq!(i("/a/b", "/a/{*r}"), Some("/a/b".into()));
}
