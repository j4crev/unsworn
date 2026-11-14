use std::future::Future;
use std::sync::Arc;

use futures_util::FutureExt;
use futures_util::future::BoxFuture;

use crate::{Reply, Request, Result};

pub(crate) type BoxHandler =
    Arc<dyn Fn(Request) -> BoxFuture<'static, Result<Reply>> + Send + Sync>;

struct Entry {
    /// The function's name, such as `get_user`.
    name: String,
    /// The module path it was registered with, such as `["users"]` for
    /// `users::get_user`.
    module: Vec<String>,
    /// Set for handlers added with [`Handlers::add_ref`].
    reference: Option<String>,
    handler: BoxHandler,
}

/// The handlers a worker serves. Build it with the [`handlers!`](crate::handlers)
/// macro, or with [`Handlers::add`] and [`Handlers::add_ref`].
///
/// A reference `src/users.rs#get_user` finds the function registered as
/// `get_user`. If several registered functions share that name, the module
/// path decides: `users::get_user` matches `src/users.rs` and
/// `src/users/mod.rs`. A reference with no `#name`, as route files use, needs
/// [`Handlers::add_ref`].
#[derive(Default)]
pub struct Handlers {
    entries: Vec<Entry>,
}

/// Lists handler functions by path: `handlers![get_user, billing::checkout]`.
#[macro_export]
macro_rules! handlers {
    ($($path:path),* $(,)?) => {{
        let handlers = $crate::Handlers::new();
        $( let handlers = handlers.add(stringify!($path), $path); )*
        handlers
    }};
}

impl Handlers {
    pub fn new() -> Handlers {
        Handlers::default()
    }

    /// Adds a handler under its Rust path, such as `"users::get_user"`.
    pub fn add<F, Fut>(mut self, path: &str, handler: F) -> Handlers
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Reply>> + Send + 'static,
    {
        let mut module: Vec<String> = path
            .split("::")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty() && s != "crate" && s != "self" && s != "super")
            .collect();
        let name = module.pop().unwrap_or_default();
        self.entries.push(Entry {
            name,
            module,
            reference: None,
            handler: boxed(handler),
        });
        self
    }

    /// Adds a handler under an explicit reference: a full reference such as
    /// `"src/users.rs#get_user"`, or a file such as
    /// `"routes/users.[id].get.rs"`.
    pub fn add_ref<F, Fut>(mut self, reference: &str, handler: F) -> Handlers
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Reply>> + Send + 'static,
    {
        self.entries.push(Entry {
            name: String::new(),
            module: Vec::new(),
            reference: Some(reference.to_string()),
            handler: boxed(handler),
        });
        self
    }

    /// Finds the handler for a reference, or explains why it can't.
    pub(crate) fn resolve(&self, reference: &str) -> std::result::Result<&BoxHandler, String> {
        let (file, name) = match reference.split_once('#') {
            Some((file, name)) if !name.is_empty() => (file, Some(name)),
            Some((file, _)) => (file, None),
            None => (reference, None),
        };
        let file = file.strip_prefix("./").unwrap_or(file);
        let wanted = match name {
            Some(name) => format!("{file}#{name}"),
            None => file.to_string(),
        };

        if let Some(e) = self.entries.iter().find(|e| {
            e.reference
                .as_deref()
                .is_some_and(|r| r.strip_prefix("./").unwrap_or(r) == wanted)
        }) {
            return Ok(&e.handler);
        }

        let Some(name) = name else {
            return Err(format!(
                "no handler is registered for {file}; Rust references need a #name, or register the file with Handlers::add_ref"
            ));
        };
        let named: Vec<&Entry> = self
            .entries
            .iter()
            .filter(|e| e.reference.is_none() && e.name == name)
            .collect();
        match named.as_slice() {
            [] => Err(format!(
                "no handler named \"{name}\" is registered; add it to unsworn_sdk::handlers![...]"
            )),
            [one] => Ok(&one.handler),
            several => {
                // The most specific module that matches the file wins, so
                // `billing::get_user` beats a top-level `get_user` for
                // src/billing.rs.
                let segments = module_of(file);
                let matching: Vec<&&Entry> = several
                    .iter()
                    .filter(|e| segments.ends_with(&e.module))
                    .collect();
                let longest = matching.iter().map(|e| e.module.len()).max().unwrap_or(0);
                let matching: Vec<&&Entry> = matching
                    .into_iter()
                    .filter(|e| e.module.len() == longest)
                    .collect();
                match matching.as_slice() {
                    [one] => Ok(&one.handler),
                    _ => Err(format!(
                        "several handlers are named \"{name}\" ({}) and none matches {file} alone",
                        several
                            .iter()
                            .map(|e| [e.module.join("::"), e.name.clone()].join("::"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )),
                }
            }
        }
    }
}

/// The module path a file would have: `src/billing/stripe.rs` is
/// `["billing", "stripe"]` (with any leading folders kept, since only the
/// end of it is compared).
fn module_of(file: &str) -> Vec<String> {
    let without_ext = file.strip_suffix(".rs").unwrap_or(file);
    let mut parts: Vec<String> = without_ext
        .split('/')
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect();
    if matches!(
        parts.last().map(String::as_str),
        Some("mod" | "main" | "lib")
    ) {
        parts.pop();
    }
    parts
}

fn boxed<F, Fut>(handler: F) -> BoxHandler
where
    F: Fn(Request) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Reply>> + Send + 'static,
{
    Arc::new(move |req| handler(req).boxed())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn get_user(_: Request) -> Result {
        Ok(Reply::text("user"))
    }

    mod billing {
        use super::*;
        pub async fn get_user(_: Request) -> Result {
            Ok(Reply::text("billing user"))
        }
    }

    mod other {
        pub use super::billing::get_user;
    }

    fn text(handlers: &Handlers, reference: &str) -> String {
        let handler = handlers.resolve(reference).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let reply = runtime.block_on(handler(Request::test()));
        match reply.unwrap().body {
            crate::Body::Text(t) => t,
            _ => panic!("not a text reply"),
        }
    }

    #[test]
    fn resolves_by_name_then_module() {
        let one = crate::handlers![get_user];
        assert!(one.resolve("src/anything.rs#get_user").is_ok());
        assert!(
            one.resolve("src/main.rs#get_usr")
                .err()
                .unwrap()
                .contains("no handler named \"get_usr\"")
        );

        let two =
            crate::handlers![get_user, billing::get_user].add_ref("routes/health.get.rs", get_user);
        assert_eq!(text(&two, "src/billing.rs#get_user"), "billing user");
        assert_eq!(text(&two, "src/billing/mod.rs#get_user"), "billing user");
        assert_eq!(text(&two, "src/main.rs#get_user"), "user");
        assert!(two.resolve("./routes/health.get.rs").is_ok());

        let clash = crate::handlers![billing::get_user, other::get_user];
        assert!(
            clash
                .resolve("src/main.rs#get_user")
                .err()
                .unwrap()
                .contains("several handlers")
        );
        assert!(
            two.resolve("routes/users.get.rs")
                .err()
                .unwrap()
                .contains("need a #name")
        );
    }

    #[test]
    fn module_paths() {
        assert_eq!(
            module_of("src/billing/stripe.rs"),
            ["src", "billing", "stripe"]
        );
        assert_eq!(module_of("src/billing/mod.rs"), ["src", "billing"]);
        assert_eq!(module_of("src/main.rs"), ["src"]);
    }
}
