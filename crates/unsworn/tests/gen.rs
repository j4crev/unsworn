//! `unsworn gen`, run as a user would, with each language's own tools
//! checking what it writes where they're installed.

mod common;

use std::process::{Command, Output};
use std::time::{Duration, Instant};

use common::Project;

/// The same two routes for a handler file in each language.
fn config(gen_table: &str) -> String {
    let mut out = format!(
        r#"
[runtimes.ts]
extensions = [".ts"]
command = "bun"

[runtimes.py]
extensions = [".py"]
command = "python3"

[runtimes.go]
extensions = [".go"]
command = "./worker"

[runtimes.rs]
extensions = [".rs"]
command = "./worker"

{gen_table}
"#
    );
    for (ext, get, create) in [
        ("ts", "getUser", "createUser"),
        ("py", "get_user", "create_user"),
        ("go", "GetUser", "CreateUser"),
        ("rs", "get_user", "create_user"),
    ] {
        out += &format!(
            r#"
[[route]]
path = "/{ext}/users/{{id}}"
method = "GET"
handler = "src/users.{ext}#{get}"
params = {{ id = {{ type = "integer", min = 1 }} }}
query = {{ limit = {{ type = "integer", default = 20 }}, q = {{ type = "string" }} }}
response = "schemas/user.json"

[[route]]
path = "/{ext}/users"
method = "POST"
handler = "src/users.{ext}#{create}"
body = "schemas/new-user.json"
"#
        );
    }
    out
}

const GEN: &str = r#"[gen]
ts = "src/unsworn.gen.ts"
python = "unsworn_gen.py"
go = { file = "go/unsworn_gen.go", package = "handlers" }
rust = "rs/unsworn_gen.rs"
"#;

const USER: &str = r#"{"type":"object","required":["id","name"],"properties":{"id":{"type":"integer"},"name":{"type":"string"},"email":{"type":["string","null"]},"type":{"type":"string"}}}"#;

const NEW_USER: &str = r##"{"type":"object","required":["name","role"],"properties":{"name":{"type":"string"},"role":{"enum":["admin","member"]},"address":{"$ref":"#/$defs/address"},"scores":{"type":"array","items":{"type":"object","required":["n"],"properties":{"n":{"type":"number"}}}}},"$defs":{"address":{"type":"object","required":["city"],"properties":{"city":{"type":"string"},"zip":{"type":"string"}}}}}"##;

fn project(gen_table: &str) -> Project {
    Project::new(&[
        ("unsworn.toml", &config(gen_table)),
        ("schemas/user.json", USER),
        ("schemas/new-user.json", NEW_USER),
        ("src/users.ts", ""),
        ("src/users.py", ""),
        ("src/users.go", ""),
        ("src/users.rs", ""),
    ])
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn writes_and_checks_every_file_in_the_gen_table() {
    let p = project(GEN);
    let out = p.command(&["gen", "--check"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("src/unsworn.gen.ts doesn't exist"),
        "{}",
        stderr(&out)
    );

    let out = p.command(&["gen"]);
    assert!(out.status.success(), "{}", stderr(&out));
    for file in [
        "src/unsworn.gen.ts",
        "unsworn_gen.py",
        "go/unsworn_gen.go",
        "rs/unsworn_gen.rs",
    ] {
        assert!(
            stdout(&out).contains(&format!("wrote {file}")),
            "{}",
            stdout(&out)
        );
        assert!(p.path().join(file).exists(), "{file}");
    }
    assert!(p.command(&["gen", "--check"]).status.success());

    // A changed contract makes the files stale.
    let changed = config(GEN).replace("min = 1", "min = 1, max = 9").replace(
        "q = { type = \"string\" }",
        "q = { type = \"string\" }, page = { type = \"integer\" }",
    );
    p.write("unsworn.toml", &changed);
    let out = p.command(&["gen", "--check"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("unsworn_gen.py is out of date"),
        "{}",
        stderr(&out)
    );
    assert!(p.command(&["gen"]).status.success());
    let ts = std::fs::read_to_string(p.path().join("src/unsworn.gen.ts")).unwrap();
    assert!(ts.contains("page?: number;"), "{ts}");
}

#[test]
fn one_language_prints_or_writes_where_its_told() {
    let p = project("");
    let out = p.command(&["gen"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("the config has no [gen] table"),
        "{}",
        stderr(&out)
    );

    let out = p.command(&["gen", "ts"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("export type GetUser = Handler<{"),
        "{}",
        stdout(&out)
    );

    let out = p.command(&["gen", "go", "-o", "x/types.go", "--package", "api"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let go = std::fs::read_to_string(p.path().join("x/types.go")).unwrap();
    assert!(go.contains("\npackage api\n"), "{go}");

    let out = p.command(&["gen", "cobol"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("use ts, python, go or rust"),
        "{}",
        stderr(&out)
    );

    // A path outside the project is refused in the config.
    p.write("unsworn.toml", &config("[gen]\nts = \"../types.ts\"\n"));
    let out = p.command(&["check"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("isn't a path inside the project"),
        "{}",
        stderr(&out)
    );
}

fn run(cmd: &mut Command) -> Output {
    cmd.output().unwrap()
}

#[test]
fn each_languages_tools_accept_the_output() {
    let p = project(GEN);
    assert!(p.command(&["gen"]).status.success());

    if common::has("gofmt") {
        let out = run(Command::new("gofmt").arg("-l").arg(p.path().join("go")));
        assert!(out.status.success(), "{}", stderr(&out));
        assert_eq!(stdout(&out), "", "gofmt would change the file");
    } else {
        eprintln!("skipped: gofmt isn't installed");
    }

    if common::has("rustfmt") {
        let out = run(Command::new("rustfmt")
            .args(["--edition", "2024", "--check"])
            .arg(p.path().join("rs/unsworn_gen.rs")));
        assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    } else {
        eprintln!("skipped: rustfmt isn't installed");
    }

    if common::has("python3") {
        // The module imports, and its types say what the contract does.
        let script = r#"
import unsworn_gen as g
assert g.GetUserParams.__required_keys__ == {"id"}
assert g.GetUserQuery.__required_keys__ == {"limit"}, g.GetUserQuery.__required_keys__
assert g.GetUserQuery.__optional_keys__ == {"q"}
assert g.CreateUserBody.__required_keys__ == {"name", "role"}
assert g.CreateUserBodyAddress.__optional_keys__ == {"zip"}
assert "GetUserParams" in str(g.GetUserRequest)
"#;
        let out = run(Command::new("python3")
            .args(["-c", script])
            .current_dir(p.path())
            .env("PYTHONPATH", common::repo_root().join("sdk/python")));
        assert!(out.status.success(), "{}", stderr(&out));
    } else {
        eprintln!("skipped: python3 isn't installed");
    }
}

/// A Go project with nothing but handlers: no `[runtimes.go]`, since Go
/// has a default, and no list of handlers kept by hand, since `gen` writes
/// one from the config.
#[test]
fn a_go_worker_is_built_from_the_generated_list() {
    if !common::has("go") {
        eprintln!("skipped: go isn't installed");
        return;
    }
    let config = r#"
[gen]
go = "unsworn_gen.go"

[[route]]
path = "/users/{id}"
method = "GET"
handler = "users.go#GetUser"
params = { id = { type = "integer", min = 1 } }
before = ["users.go#Tenant"]
"#;
    let users = r#"package main

import "github.com/j4crev/unsworn/sdk/go/unsworn"

func GetUser(req *unsworn.Request) (unsworn.Reply, error) {
	var p GetUserParams
	if err := req.BindParams(&p); err != nil {
		return unsworn.Reply{}, err
	}
	return unsworn.JSON(map[string]any{"id": p.ID, "tenant": req.Ctx["tenant"]}), nil
}

func Tenant(req *unsworn.Request) (unsworn.Reply, error) {
	return unsworn.Next(map[string]any{"tenant": "acme"}), nil
}
"#;
    // All of `main` there is, and it never changes.
    let main = "package main\n\nimport \"github.com/j4crev/unsworn/sdk/go/unsworn\"\n\nfunc main() { unsworn.Serve(Handlers()...) }\n";
    let module = format!(
        "module example.com/app\n\ngo 1.22\n\nrequire github.com/j4crev/unsworn/sdk/go v0.0.0\n\nreplace github.com/j4crev/unsworn/sdk/go => {}\n",
        common::repo_root().join("sdk/go").display()
    );
    let p = Project::new(&[
        ("unsworn.toml", config),
        ("users.go", users),
        ("main.go", main),
        ("go.mod", &module),
    ]);
    let out = p.command(&["gen"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let generated = std::fs::read_to_string(p.path().join("unsworn_gen.go")).unwrap();
    assert!(
        generated.contains("func Handlers() []unsworn.HandlerFunc {\n\treturn []unsworn.HandlerFunc{\n\t\tGetUser,\n\t\tTenant,\n\t}\n}\n"),
        "{generated}"
    );
    if common::has("gofmt") {
        let out = run(Command::new("gofmt").arg("-l").arg(p.path()));
        assert_eq!(stdout(&out), "", "gofmt would change a file");
    }

    // The default runtime builds the worker, and the worker has both
    // handlers the config names.
    let out = p.command(&["check", "--deep"]);
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    let s = p.serve();
    let r = s.get("/users/7");
    assert_eq!(r.status, 200, "{}\n{}", r.text(), s.logs());
    assert_eq!(r.json(), serde_json::json!({ "id": 7, "tenant": "acme" }));
}

/// `unsworn dev` keeps the files current: at startup, and when the config
/// changes.
#[test]
fn dev_regenerates_on_reload() {
    if !common::has("python3") {
        eprintln!("skipped: python3 isn't installed");
        return;
    }
    let worker =
        std::fs::read_to_string(common::repo_root().join("examples/raw-python/worker.py")).unwrap();
    let config = |query: &str| {
        format!(
            r#"
[runtimes.raw]
extensions = [".ts"]
command = "python3 -u worker.py"

[gen]
ts = "src/unsworn.gen.ts"

[[route]]
path = "/items"
method = "GET"
handler = "src/items.ts#list"
query = {{ {query} }}
"#
        )
    };
    let p = Project::new(&[
        ("unsworn.toml", &config("limit = { type = \"integer\" }")),
        ("worker.py", &worker),
        ("src/items.ts", ""),
    ]);
    let server = p.start("dev");
    let file = p.path().join("src/unsworn.gen.ts");
    let first = std::fs::read_to_string(&file).expect("written at startup");
    assert!(first.contains("limit?: number;"), "{first}");
    assert!(
        server.logs().contains("wrote src/unsworn.gen.ts"),
        "{}",
        server.logs()
    );

    p.write(
        "unsworn.toml",
        &config("limit = { type = \"integer\" }, after = { type = \"string\" }"),
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let now = std::fs::read_to_string(&file).unwrap_or_default();
        if now.contains("after?: string;") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "not regenerated:\n{now}\n{}",
            server.logs()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The example's committed types match its config.
#[test]
fn the_polyglot_examples_types_are_current() {
    let out = Command::new(env!("CARGO_BIN_EXE_unsworn"))
        .arg("-C")
        .arg(common::repo_root().join("examples/polyglot"))
        .args(["gen", "--check"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
}
