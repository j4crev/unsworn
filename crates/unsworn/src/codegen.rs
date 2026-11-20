//! `gen`: typed request models for handlers, written from the config.

use std::path::Path;

use unsworn_core::codegen::{Lang, Options, generate};
use unsworn_core::project::Generated;
use unsworn_core::{Project, compile_symbolic};

use crate::load::{self, FsHost};

/// What writing one generated file came to.
pub enum Wrote {
    /// The file already said this.
    Unchanged,
    Written,
}

fn write(root: &Path, file: &str, text: &str) -> Result<Wrote, String> {
    let path = root.join(file);
    if std::fs::read_to_string(&path).is_ok_and(|current| current == text) {
        return Ok(Wrote::Unchanged);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    }
    std::fs::write(&path, text).map_err(|e| format!("can't write {}: {e}", path.display()))?;
    Ok(Wrote::Written)
}

/// Writes every file `[gen]` lists that isn't up to date. Returns the
/// files written. `unsworn dev` calls this on each load, before workers
/// are built, so a compiled worker is built with its new types.
pub fn write_all(root: &Path, project: &Project) -> Result<Vec<String>, String> {
    let mut written = Vec::new();
    for g in &project.generated {
        let text = generate(project, g.lang, &g.options);
        if let Wrote::Written = write(root, &g.file, &text)? {
            written.push(g.file.clone());
        }
    }
    Ok(written)
}

pub fn run(
    root: &Path,
    config: &str,
    lang: Option<&str>,
    output: Option<&str>,
    package: Option<&str>,
    check: bool,
) -> Result<(), String> {
    let host = FsHost {
        root: root.to_path_buf(),
    };
    // Types don't depend on the environment, so none of it is needed.
    let compiled = compile_symbolic(&host, config);
    let errors: Vec<_> = compiled
        .diagnostics
        .into_iter()
        .filter(|d| d.is_error())
        .collect();
    load::print_diagnostics(&errors);
    let Some(project) = compiled.project else {
        return Err(format!("the project has {} error(s)", errors.len()));
    };

    let targets: Vec<(Generated, bool)> = match lang {
        None => {
            if output.is_some() || package.is_some() {
                return Err("--output and --package go with one language, as in `unsworn gen ts -o types.ts`".into());
            }
            if project.generated.is_empty() {
                return Err(
                    "nothing to generate: the config has no [gen] table\n  → add one, such as\n      [gen]\n      ts = \"src/unsworn.gen.ts\"\n    or name a language to print its types: `unsworn gen ts`"
                        .into(),
                );
            }
            project
                .generated
                .iter()
                .map(|g| (g.clone(), false))
                .collect()
        }
        Some(name) => {
            let lang = Lang::parse(name).ok_or_else(|| {
                format!("\"{name}\" isn't a language `gen` knows: use ts, python, go or rust")
            })?;
            let listed = project.generated.iter().find(|g| g.lang == lang);
            let file = output
                .map(String::from)
                .or_else(|| listed.map(|g| g.file.clone()));
            let options = Options {
                go_package: package
                    .map(String::from)
                    .or_else(|| listed.and_then(|g| g.options.go_package.clone())),
                // Where the file is says which handlers it can name.
                file: file.clone().filter(|f| f != "-"),
            };
            match file.as_deref() {
                None | Some("-") => {
                    if check {
                        return Err(format!(
                            "--check compares with a file: pass `-o <file>`, or list {name} in [gen]"
                        ));
                    }
                    print!("{}", generate(&project, lang, &options));
                    return Ok(());
                }
                Some(file) => vec![(
                    Generated {
                        lang,
                        file: file.to_string(),
                        options,
                    },
                    true,
                )],
            }
        }
    };

    let mut stale = 0;
    for (g, _) in &targets {
        let text = generate(&project, g.lang, &g.options);
        if check {
            let current = std::fs::read_to_string(root.join(&g.file)).ok();
            if current.as_deref() == Some(text.as_str()) {
                println!("✓ {} is up to date", g.file);
            } else {
                eprintln!(
                    "✗ {} {}",
                    g.file,
                    if current.is_some() {
                        "is out of date"
                    } else {
                        "doesn't exist"
                    }
                );
                stale += 1;
            }
            continue;
        }
        match write(root, &g.file, &text)? {
            Wrote::Unchanged => println!("✓ {} is up to date", g.file),
            Wrote::Written => println!("✓ wrote {}", g.file),
        }
    }
    if stale > 0 {
        eprintln!("  → run `unsworn gen` and commit the result");
        return Err(String::new());
    }
    Ok(())
}
