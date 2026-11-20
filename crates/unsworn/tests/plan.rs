//! `unsworn plan` and `unsworn diff`, run as a user would.

mod common;

use std::path::Path;
use std::process::{Command, Output};

use common::Project;

const CONFIG: &str = r#"
[runtimes.ts]
extensions = [".ts"]
command = "bun run worker.ts"

[auth.jwt]
type = "jwt"
secret = "{env.PLAN_TEST_SECRET_THAT_IS_NOT_SET}"

[defaults]
auth = "jwt"

[[route]]
path = "/{*rest}"
proxy = "{env.PLAN_TEST_APP_THAT_IS_NOT_SET}"

[[route]]
path = "/users/{id}"
method = "GET"
handler = "src/users.ts#get"
params = { id = { type = "integer" } }
"#;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=test", "-c", "user.email=test@example.com"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn commit_all(dir: &Path) {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "x"]);
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn plan_is_written_and_checked() {
    let p = Project::new(&[("unsworn.toml", CONFIG), ("src/users.ts", "")]);

    let out = p.command(&["plan", "--check"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("unsworn.plan doesn't exist"),
        "{}",
        stderr(&out)
    );

    // No variable the config names is set, and none is needed.
    let out = p.command(&["plan"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("wrote unsworn.plan"));
    let plan = std::fs::read_to_string(p.path().join("unsworn.plan")).unwrap();
    assert!(plan.contains("\nGET /users/{id}\n  handler            src/users.ts#get (ts)\n  auth               jwt\n"), "{plan}");
    assert!(
        plan.contains("{env.PLAN_TEST_APP_THAT_IS_NOT_SET}"),
        "{plan}"
    );
    assert!(!plan.contains("PLAN_TEST_SECRET"), "{plan}");

    assert!(p.command(&["plan", "--check"]).status.success());
    assert!(stdout(&p.command(&["plan"])).contains("is up to date"));
    assert_eq!(stdout(&p.command(&["plan", "-o", "-"])), plan);

    p.write(
        "unsworn.toml",
        &CONFIG.replace("auth = \"jwt\"\n", "auth = \"none\"\n"),
    );
    let out = p.command(&["plan", "--check"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("unsworn.plan is out of date"),
        "{}",
        stderr(&out)
    );
    assert!(stdout(&p.command(&["plan"])).contains("updated unsworn.plan"));

    // An invalid config has no plan.
    p.write(
        "unsworn.toml",
        "[[route]]\npath = \"/x\"\nhandler = \"missing.ts\"\n",
    );
    let out = p.command(&["plan"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("the project has"), "{}", stderr(&out));
}

#[test]
fn diff_against_a_git_revision() {
    if !common::has("git") {
        eprintln!("skipped: git isn't installed");
        return;
    }
    let p = Project::new(&[("unsworn.toml", CONFIG), ("src/users.ts", "")]);
    git(p.path(), &["init", "-q", "-b", "main"]);
    commit_all(p.path());

    let out = p.command(&["diff"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), "The rules are the same.\n");
    assert!(p.command(&["diff", "--fail-on", "any"]).status.success());

    // Take the token requirement off, and add a route with a new file.
    let open = CONFIG.replace("auth = \"jwt\"\n", "auth = \"none\"\n")
        + "\n[[route]]\npath = \"/reports\"\nmethod = \"POST\"\nhandler = \"src/reports.ts\"\n";
    p.write("unsworn.toml", &open);
    p.write("src/reports.ts", "");
    let out = p.command(&["diff"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains("~ GET /users/{id}\n  ! auth               jwt → none  (looser)\n"),
        "{text}"
    );
    assert!(
        text.contains("~ * /{*rest}\n  ! auth               jwt → none  (looser)\n"),
        "{text}"
    );
    // A new route that anyone may call is marked as the looser rules are.
    assert!(text.contains("+ POST /reports\n    handler            src/reports.ts (ts)\n  ! auth               none  (looser: anyone may call it)\n"), "{text}");
    assert!(
        text.ends_with("\n2 changes (2 looser), 1 added (1 open to anyone)\n"),
        "{text}"
    );

    let out = p.command(&["diff", "main", "--fail-on", "looser"]);
    assert!(!out.status.success());
    assert_eq!(stdout(&out), text);

    // Committed, the working tree matches HEAD again, but not the commit
    // before it.
    commit_all(p.path());
    assert_eq!(stdout(&p.command(&["diff"])), "The rules are the same.\n");
    assert_eq!(stdout(&p.command(&["diff", "HEAD~1"])), text);

    let out = p.command(&["diff", "no-such-branch"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("isn't a directory, or a git revision"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn diff_in_a_subdirectory_of_the_repository_and_against_a_directory() {
    if !common::has("git") {
        eprintln!("skipped: git isn't installed");
        return;
    }
    let p = Project::new(&[
        ("api/unsworn.toml", CONFIG),
        ("api/src/users.ts", ""),
        ("api/routes/health.get.ts", ""),
        ("README.md", "not the project"),
    ]);
    git(p.path(), &["init", "-q", "-b", "main"]);
    commit_all(p.path());
    let api = p.path().join("api");
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_unsworn"))
            .arg("-C")
            .arg(&api)
            .args(args)
            .output()
            .unwrap()
    };
    assert_eq!(stdout(&run(&["diff"])), "The rules are the same.\n");

    // A routes directory is listed from the revision, not from disk.
    let with_dir = format!("{CONFIG}\n[routes]\ndir = \"routes\"\n");
    std::fs::write(api.join("unsworn.toml"), &with_dir).unwrap();
    let text = stdout(&run(&["diff"]));
    assert!(text.contains("+ GET /health\n"), "{text}");
    commit_all(p.path());
    std::fs::remove_file(api.join("routes/health.get.ts")).unwrap();
    std::fs::write(api.join("routes/ping.get.ts"), "").unwrap();
    let text = stdout(&run(&["diff"]));
    assert!(text.contains("- GET /health\n"), "{text}");
    assert!(text.contains("+ GET /ping\n"), "{text}");

    // Another copy of the project works as the base too.
    let other = Project::new(&[("unsworn.toml", CONFIG), ("src/users.ts", "")]);
    let text = stdout(&run(&["diff", other.path().to_str().unwrap()]));
    assert!(text.contains("+ GET /ping\n"), "{text}");
    assert!(!text.contains("/health"), "{text}");
}

/// The example's committed plan matches its config.
#[test]
fn the_polyglot_examples_plan_is_current() {
    let out = Command::new(env!("CARGO_BIN_EXE_unsworn"))
        .arg("-C")
        .arg(common::repo_root().join("examples/polyglot"))
        .args(["plan", "--check"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
}
