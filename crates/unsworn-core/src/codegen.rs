//! Typed request models for handlers, generated from each route's contract.
//!
//! A route's `params`, `query`, `body` and `response` are JSON Schemas that
//! Unsworn checks before the handler runs. A handler then reads values it
//! knows the shape of only from the config. This module writes those
//! shapes as types in the handler's own language, so the compiler knows
//! them too, and a config change that breaks a handler is a type error.
//!
//! [`generate`] covers the handlers one language serves, picked by file
//! extension. What a language can't say, such as a union in Go, becomes
//! its "anything" type.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde_json::Value;

use crate::project::{Action, Project, Route};
use crate::validate::{BodyRule, LocationRules};

/// A language types can be generated for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Lang {
    TypeScript,
    Python,
    Go,
    Rust,
}

impl Lang {
    pub const ALL: [Lang; 4] = [Lang::TypeScript, Lang::Python, Lang::Go, Lang::Rust];

    /// The name in `[gen]` and on the command line.
    pub fn name(self) -> &'static str {
        match self {
            Lang::TypeScript => "ts",
            Lang::Python => "python",
            Lang::Go => "go",
            Lang::Rust => "rust",
        }
    }

    pub fn parse(name: &str) -> Option<Lang> {
        match name {
            "ts" | "typescript" => Some(Lang::TypeScript),
            "py" | "python" => Some(Lang::Python),
            "go" => Some(Lang::Go),
            "rs" | "rust" => Some(Lang::Rust),
            _ => None,
        }
    }

    /// The language of a handler file, by its extension.
    pub fn of_file(path: &str) -> Option<Lang> {
        let ext = path.rsplit_once('.')?.1;
        match ext {
            "ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs" => Some(Lang::TypeScript),
            "py" => Some(Lang::Python),
            "go" => Some(Lang::Go),
            "rs" => Some(Lang::Rust),
            _ => None,
        }
    }
}

/// A type, as far as every target language can be told about it.
#[derive(Debug, Clone, PartialEq)]
enum Ty {
    Any,
    Null,
    Bool,
    Int,
    Num,
    Str,
    /// One exact value, from `const` or `enum`.
    Lit(Value),
    Array(Box<Ty>),
    Tuple(Vec<Ty>),
    /// An object with any keys, each holding this type.
    Map(Box<Ty>),
    Object(Vec<Field>),
    Union(Vec<Ty>),
    /// Every one of these at once (`allOf`), where they couldn't be merged.
    All(Vec<Ty>),
}

#[derive(Debug, Clone, PartialEq)]
struct Field {
    name: String,
    ty: Ty,
    required: bool,
}

/// How deep `$ref`s and nesting are followed. A schema that refers to
/// itself stops here, as "anything".
const MAX_DEPTH: usize = 24;

fn ty(schema: &Value, root: &Value, depth: usize) -> Ty {
    let Value::Object(map) = schema else {
        return Ty::Any;
    };
    if depth > MAX_DEPTH {
        return Ty::Any;
    }
    if let Some(Value::String(r)) = map.get("$ref") {
        return match r.strip_prefix('#').and_then(|p| root.pointer(p)) {
            Some(target) => ty(target, root, depth + 1),
            None => Ty::Any,
        };
    }
    let nullable = map.get("nullable") == Some(&Value::Bool(true));
    let out = ty_of(map, root, depth);
    if nullable {
        union(vec![out, Ty::Null])
    } else {
        out
    }
}

fn ty_of(map: &serde_json::Map<String, Value>, root: &Value, depth: usize) -> Ty {
    if let Some(v) = map.get("const") {
        return Ty::Lit(v.clone());
    }
    if let Some(Value::Array(values)) = map.get("enum") {
        return union(values.iter().cloned().map(Ty::Lit).collect());
    }
    for key in ["oneOf", "anyOf"] {
        if let Some(Value::Array(list)) = map.get(key) {
            return union(list.iter().map(|s| ty(s, root, depth + 1)).collect());
        }
    }
    if let Some(Value::Array(list)) = map.get("allOf") {
        let mut parts: Vec<Ty> = list.iter().map(|s| ty(s, root, depth + 1)).collect();
        // Keywords next to `allOf` apply as well.
        let mut own = map.clone();
        own.remove("allOf");
        if own.contains_key("properties") || own.contains_key("type") {
            parts.push(ty_of(&own, root, depth));
        }
        return all(parts);
    }
    match map.get("type") {
        Some(Value::String(name)) => typed(name, map, root, depth),
        Some(Value::Array(names)) => union(
            names
                .iter()
                .filter_map(Value::as_str)
                .map(|n| typed(n, map, root, depth))
                .collect(),
        ),
        _ if map.contains_key("properties") || map.contains_key("additionalProperties") => {
            typed("object", map, root, depth)
        }
        _ if map.contains_key("items") || map.contains_key("prefixItems") => {
            typed("array", map, root, depth)
        }
        _ => Ty::Any,
    }
}

fn typed(name: &str, map: &serde_json::Map<String, Value>, root: &Value, depth: usize) -> Ty {
    match name {
        "string" => Ty::Str,
        "integer" => Ty::Int,
        "number" => Ty::Num,
        "boolean" => Ty::Bool,
        "null" => Ty::Null,
        "array" => match (map.get("prefixItems"), map.get("items")) {
            (Some(Value::Array(items)), _) => {
                Ty::Tuple(items.iter().map(|s| ty(s, root, depth + 1)).collect())
            }
            (_, Some(items)) => Ty::Array(Box::new(ty(items, root, depth + 1))),
            _ => Ty::Array(Box::new(Ty::Any)),
        },
        "object" => {
            let required: Vec<&str> = map
                .get("required")
                .and_then(Value::as_array)
                .map(|list| list.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let fields: Vec<Field> = map
                .get("properties")
                .and_then(Value::as_object)
                .map(|props| {
                    props
                        .iter()
                        .map(|(name, schema)| Field {
                            name: name.clone(),
                            ty: ty(schema, root, depth + 1),
                            required: required.contains(&name.as_str()),
                        })
                        .collect()
                })
                .unwrap_or_default();
            if !fields.is_empty() {
                return Ty::Object(fields);
            }
            match map.get("additionalProperties") {
                Some(extra @ Value::Object(_)) => Ty::Map(Box::new(ty(extra, root, depth + 1))),
                Some(Value::Bool(false)) => Ty::Object(Vec::new()),
                _ => Ty::Map(Box::new(Ty::Any)),
            }
        }
        _ => Ty::Any,
    }
}

/// A union without repeats, or the one type it comes down to.
fn union(parts: Vec<Ty>) -> Ty {
    let mut out: Vec<Ty> = Vec::new();
    for part in parts {
        let members = match part {
            Ty::Union(inner) => inner,
            other => vec![other],
        };
        for m in members {
            if m == Ty::Any {
                return Ty::Any;
            }
            if !out.contains(&m) {
                out.push(m);
            }
        }
    }
    match out.len() {
        0 => Ty::Any,
        1 => out.remove(0),
        _ => Ty::Union(out),
    }
}

/// `allOf`: objects are merged into one; anything else is kept apart.
fn all(parts: Vec<Ty>) -> Ty {
    let parts: Vec<Ty> = parts.into_iter().filter(|p| *p != Ty::Any).collect();
    if parts.iter().all(|p| matches!(p, Ty::Object(_))) && !parts.is_empty() {
        let mut fields: Vec<Field> = Vec::new();
        for part in parts {
            let Ty::Object(more) = part else { continue };
            for f in more {
                match fields.iter_mut().find(|x| x.name == f.name) {
                    Some(existing) => existing.required |= f.required,
                    None => fields.push(f),
                }
            }
        }
        return Ty::Object(fields);
    }
    match parts.len() {
        0 => Ty::Any,
        1 => parts.into_iter().next().unwrap_or(Ty::Any),
        _ => Ty::All(parts),
    }
}

/// What one route gives its handler, and expects back.
#[derive(Debug, Clone, PartialEq, Default)]
struct Shape {
    params: Option<Ty>,
    query: Option<Ty>,
    body: Option<Ty>,
    /// The body may be missing.
    body_optional: bool,
    response: Option<Ty>,
}

/// A handler and the shapes of the routes it serves.
#[derive(Debug)]
struct Unit {
    /// The base of its type names, such as `GetUser`.
    name: String,
    /// Each distinct shape, with the routes that have it and, when there
    /// are several, a suffix that tells their names apart.
    shapes: Vec<(String, Vec<String>, Shape)>,
}

fn location(rules: &Option<LocationRules>, path_params: &[String]) -> Option<Ty> {
    let mut fields: Vec<Field> = Vec::new();
    for spec in rules.iter().flat_map(|r| &r.specs) {
        let always = !path_params.is_empty();
        fields.push(Field {
            name: spec.name.clone(),
            ty: ty(&spec.schema, &spec.schema, 0),
            // A parameter with a default is always there.
            required: always || spec.required || spec.schema.get("default").is_some(),
        });
    }
    // A path parameter without a rule arrives as the text in the path.
    for name in path_params {
        if !fields.iter().any(|f| &f.name == name) {
            fields.push(Field {
                name: name.clone(),
                ty: Ty::Str,
                required: true,
            });
        }
    }
    (!fields.is_empty()).then_some(Ty::Object(fields))
}

fn body(rule: &Option<BodyRule>) -> Option<Ty> {
    rule.as_ref().map(|r| ty(&r.schema, &r.schema, 0))
}

fn shape_of(project: &Project, route: &Route) -> Shape {
    if let Some(tool) = route
        .mcp_tool()
        .and_then(|name| project.mcp.as_ref()?.tool(name))
    {
        return Shape {
            body: Some(ty(&tool.input_schema, &tool.input_schema, 0)),
            response: tool.output_schema.as_ref().map(|s| ty(s, s, 0)),
            ..Shape::default()
        };
    }
    let v = &route.validation;
    Shape {
        params: location(&v.params, &route.param_names),
        query: location(&v.query, &[]),
        body: body(&v.body),
        body_optional: v.body.as_ref().is_some_and(|b| !b.required),
        response: body(&v.response),
    }
}

/// A `[[function]]`, as a caller in any language sees it.
struct Callable {
    name: String,
    input: Option<Ty>,
    output: Option<Ty>,
}

fn callables(project: &Project) -> Vec<Callable> {
    project
        .functions
        .values()
        .map(|f| Callable {
            name: f.name.clone(),
            input: body(&f.input),
            output: body(&f.output),
        })
        .collect()
}

fn route_title(route: &Route) -> String {
    match route.mcp_tool() {
        Some(name) => format!("the MCP tool {name}"),
        None => format!("{} {}", route.method_label(), route.path),
    }
}

/// `get_user`, `getUser` and `api.users.[id].get` as `GetUser` and
/// `ApiUsersIdGet`.
fn pascal(text: &str) -> String {
    let mut out = String::new();
    let mut upper = true;
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            if upper {
                out.push(c.to_ascii_uppercase());
            } else {
                out.push(c);
            }
            upper = false;
        } else {
            upper = true;
        }
    }
    if out.chars().next().is_none_or(|c| c.is_ascii_digit()) {
        out.insert(0, 'T');
    }
    out
}

fn file_stem(file: &str) -> &str {
    let name = file.rsplit('/').next().unwrap_or(file);
    name.rsplit_once('.').map_or(name, |(stem, _)| stem)
}

fn units(project: &Project, lang: Lang) -> Vec<Unit> {
    // Handler reference -> (file, name, shapes by route).
    let mut found: BTreeMap<String, (String, Option<String>, Vec<(String, Shape)>)> =
        BTreeMap::new();
    for route in &project.routes {
        let Action::Handler(h) = &route.action else {
            continue;
        };
        if Lang::of_file(&h.file) != Some(lang) {
            continue;
        }
        found
            .entry(h.raw.clone())
            .or_insert_with(|| (h.file.clone(), h.name.clone(), Vec::new()))
            .2
            .push((route_title(route), shape_of(project, route)));
    }
    // A function's handler gets its input as the body.
    for f in project.functions.values() {
        let h = &f.handler;
        if Lang::of_file(&h.file) != Some(lang) {
            continue;
        }
        let shape = Shape {
            body: body(&f.input),
            response: body(&f.output),
            ..Shape::default()
        };
        found
            .entry(h.raw.clone())
            .or_insert_with(|| (h.file.clone(), h.name.clone(), Vec::new()))
            .2
            .push((format!("the function {}", f.name), shape));
    }

    // A name from the handler's own, or its file's. Handlers that would
    // share one get their file's name in front.
    let base = |file: &str, name: &Option<String>| match name {
        Some(n) => pascal(n),
        None => pascal(file_stem(file)),
    };
    let mut count: BTreeMap<String, usize> = BTreeMap::new();
    for (file, name, _) in found.values() {
        *count.entry(base(file, name)).or_default() += 1;
    }
    let mut taken: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for (file, name, routes) in found.into_values() {
        let mut unit_name = base(&file, &name);
        if count[&unit_name] > 1 && name.is_some() {
            unit_name = format!("{}{unit_name}", pascal(file_stem(&file)));
        }
        let wanted = unit_name.clone();
        let mut n = 2;
        while taken.contains(&unit_name) {
            unit_name = format!("{wanted}{n}");
            n += 1;
        }
        taken.push(unit_name.clone());

        let mut shapes: Vec<(String, Vec<String>, Shape)> = Vec::new();
        for (title, shape) in routes {
            match shapes.iter_mut().find(|(_, _, s)| *s == shape) {
                Some((_, titles, _)) => titles.push(title),
                None => shapes.push((String::new(), vec![title], shape)),
            }
        }
        if shapes.len() > 1 {
            for (i, (suffix, titles, _)) in shapes.iter_mut().enumerate() {
                // `GET /items/{id}` as `GetItemsId`; a number if that isn't
                // enough to tell two apart.
                *suffix = pascal(&titles[0]);
                if suffix.is_empty() || titles[0].starts_with("the ") {
                    *suffix = format!("{}", i + 1);
                }
            }
            let suffixes: Vec<String> = shapes.iter().map(|(s, _, _)| s.clone()).collect();
            for (i, (suffix, _, _)) in shapes.iter_mut().enumerate() {
                if suffixes.iter().filter(|s| *s == suffix).count() > 1 {
                    *suffix = format!("{suffix}{}", i + 1);
                }
            }
        }
        out.push(Unit {
            name: unit_name,
            shapes,
        });
    }
    out
}

/// Settings a language needs beyond the project.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Options {
    /// The Go package the file belongs to. Default: `main`.
    pub go_package: Option<String>,
}

/// The types for every handler written in `lang`, as one source file.
pub fn generate(project: &Project, lang: Lang, options: &Options) -> String {
    let units = units(project, lang);
    // Every language gets every function: any handler may call one.
    let calls = callables(project);
    let from = &project.config_file;
    match lang {
        Lang::TypeScript => {
            let services: Vec<&String> = project.outbound.keys().collect();
            typescript(&units, &calls, &services, from)
        }
        Lang::Python => python(&units, &calls, from),
        Lang::Go => go(
            &units,
            &calls,
            from,
            options.go_package.as_deref().unwrap_or("main"),
        ),
        Lang::Rust => rust(&units, &calls, from),
    }
}

fn routes_text(titles: &[String]) -> String {
    titles.join(", ")
}

// ---------------------------------------------------------------- TypeScript

fn ts_key(name: &str) -> String {
    let plain = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
        && !name.chars().next().is_none_or(|c| c.is_ascii_digit());
    if plain {
        name.to_string()
    } else {
        Value::from(name).to_string()
    }
}

fn ts(t: &Ty, indent: usize) -> String {
    match t {
        Ty::Any => "unknown".into(),
        Ty::Null => "null".into(),
        Ty::Bool => "boolean".into(),
        Ty::Int | Ty::Num => "number".into(),
        Ty::Str => "string".into(),
        Ty::Lit(v) => v.to_string(),
        Ty::Array(inner) => match **inner {
            Ty::Union(_) | Ty::All(_) => format!("({})[]", ts(inner, indent)),
            _ => format!("{}[]", ts(inner, indent)),
        },
        Ty::Tuple(items) => {
            let parts: Vec<String> = items.iter().map(|i| ts(i, indent)).collect();
            format!("[{}]", parts.join(", "))
        }
        Ty::Map(inner) => format!("Record<string, {}>", ts(inner, indent)),
        Ty::Object(fields) if fields.is_empty() => "Record<string, never>".into(),
        Ty::Object(fields) => {
            let pad = "  ".repeat(indent + 1);
            let mut out = String::from("{\n");
            for f in fields {
                let _ = writeln!(
                    out,
                    "{pad}{}{}: {};",
                    ts_key(&f.name),
                    if f.required { "" } else { "?" },
                    ts(&f.ty, indent + 1)
                );
            }
            let _ = write!(out, "{}}}", "  ".repeat(indent));
            out
        }
        Ty::Union(parts) => {
            let parts: Vec<String> = parts.iter().map(|p| ts(p, indent)).collect();
            parts.join(" | ")
        }
        Ty::All(parts) => {
            let parts: Vec<String> = parts
                .iter()
                .map(|p| match p {
                    Ty::Union(_) => format!("({})", ts(p, indent)),
                    _ => ts(p, indent),
                })
                .collect();
            parts.join(" & ")
        }
    }
}

fn ts_shape(shape: &Shape, indent: usize) -> String {
    let pad = "  ".repeat(indent + 1);
    let mut out = String::from("{\n");
    if let Some(p) = &shape.params {
        let _ = writeln!(out, "{pad}params: {};", ts(p, indent + 1));
    }
    if let Some(q) = &shape.query {
        // Query parameters without a rule arrive too, as text.
        let _ = writeln!(
            out,
            "{pad}query: {} & Record<string, unknown>;",
            ts(q, indent + 1)
        );
    }
    if let Some(b) = &shape.body {
        let optional = if shape.body_optional {
            " | undefined"
        } else {
            ""
        };
        let _ = writeln!(out, "{pad}json: {}{optional};", ts(b, indent + 1));
    }
    let _ = write!(out, "{}}}", "  ".repeat(indent));
    out
}

fn typescript(units: &[Unit], calls: &[Callable], services: &[&String], from: &str) -> String {
    let mut out = format!(
        "// Generated by `unsworn gen ts` from {from}. Don't edit it: change the\n\
         // config, or a schema it names, and generate it again.\n\n\
         import type {{ Handler }} from \"@unswornjs/sdk\";\n"
    );
    for unit in units {
        let titles: Vec<String> = unit
            .shapes
            .iter()
            .flat_map(|(_, t, _)| t.iter().cloned())
            .collect();
        let _ = writeln!(out, "\n/** {} */", routes_text(&titles));
        let shapes: Vec<&Shape> = unit
            .shapes
            .iter()
            .map(|(_, _, s)| s)
            .filter(|s| s.params.is_some() || s.query.is_some() || s.body.is_some())
            .collect();
        match shapes.len() {
            0 => {
                let _ = writeln!(out, "export type {} = Handler;", unit.name);
            }
            1 if unit.shapes.len() == 1 => {
                let _ = writeln!(
                    out,
                    "export type {} = Handler<{}>;",
                    unit.name,
                    ts_shape(shapes[0], 0)
                );
            }
            _ => {
                let _ = writeln!(out, "export type {} = Handler<", unit.name);
                for (_, _, shape) in &unit.shapes {
                    let _ = writeln!(out, "  | {}", ts_shape(shape, 1));
                }
                out.push_str(">;\n");
            }
        }
        for (suffix, titles, shape) in &unit.shapes {
            if let Some(r) = &shape.response {
                let _ = writeln!(
                    out,
                    "\n/** What {} answers with. */\nexport type {}{suffix}Response = {};",
                    routes_text(titles),
                    unit.name,
                    ts(r, 0)
                );
            }
        }
    }
    if !services.is_empty() {
        out.push_str(
            "\n// The services `ctx.fetch` can reach: each `[outbound.<name>]`.\n\
             declare module \"@unswornjs/sdk\" {\n  interface Services {\n",
        );
        for name in services {
            let _ = writeln!(out, "    {}: true;", ts_key(name));
        }
        out.push_str("  }\n}\n");
    }
    if !calls.is_empty() {
        out.push_str(
            "\n// What `ctx.call` takes and gives back, for each `[[function]]`.\n\
             declare module \"@unswornjs/sdk\" {\n  interface Functions {\n",
        );
        for c in calls {
            let side = |t: &Option<Ty>| t.as_ref().map_or("unknown".into(), |t| ts(t, 3));
            let _ = writeln!(
                out,
                "    {}: {{\n      input: {};\n      output: {};\n    }};",
                ts_key(&c.name),
                side(&c.input),
                side(&c.output)
            );
        }
        out.push_str("  }\n}\n");
    }
    out
}

// -------------------------------------------------------------------- Python

const PY_KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield",
];

fn py_ident(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !name.chars().next().is_some_and(|c| c.is_ascii_digit())
        && !PY_KEYWORDS.contains(&name)
}

/// Writes Python type expressions, collecting the classes they need.
#[derive(Default)]
struct Py {
    /// Class and alias definitions, in the order they must appear.
    defs: Vec<String>,
}

impl Py {
    fn expr(&mut self, t: &Ty, name: &str) -> String {
        match t {
            Ty::Any | Ty::All(_) => "Any".into(),
            Ty::Null => "None".into(),
            Ty::Bool => "bool".into(),
            Ty::Int => "int".into(),
            Ty::Num => "float".into(),
            Ty::Str => "str".into(),
            Ty::Lit(Value::String(s)) => format!("Literal[{}]", Value::from(s.as_str())),
            Ty::Lit(Value::Bool(b)) => format!("Literal[{}]", if *b { "True" } else { "False" }),
            Ty::Lit(Value::Number(n)) if n.is_i64() || n.is_u64() => format!("Literal[{n}]"),
            Ty::Lit(Value::Number(_)) => "float".into(),
            Ty::Lit(Value::Null) => "None".into(),
            Ty::Lit(_) => "Any".into(),
            Ty::Array(inner) => format!("list[{}]", self.expr(inner, &format!("{name}Item"))),
            // JSON arrays arrive as lists, whatever their items.
            Ty::Tuple(_) => "list[Any]".into(),
            Ty::Map(inner) => format!("dict[str, {}]", self.expr(inner, &format!("{name}Value"))),
            Ty::Object(fields) => {
                self.class(name, fields);
                name.to_string()
            }
            Ty::Union(parts) => {
                let mut out: Vec<String> = Vec::new();
                let mut literals: Vec<String> = Vec::new();
                for (i, p) in parts.iter().enumerate() {
                    let e = self.expr(p, &format!("{name}{}", i + 1));
                    // Several exact values read better as one `Literal`.
                    match e.strip_prefix("Literal[").and_then(|l| l.strip_suffix(']')) {
                        Some(value) => literals.push(value.to_string()),
                        None if !out.contains(&e) => out.push(e),
                        None => {}
                    }
                }
                if !literals.is_empty() {
                    out.insert(0, format!("Literal[{}]", literals.join(", ")));
                }
                out.join(" | ")
            }
        }
    }

    fn class(&mut self, name: &str, fields: &[Field]) {
        let typed: Vec<(&Field, String)> = fields
            .iter()
            .map(|f| (f, self.expr(&f.ty, &format!("{name}{}", pascal(&f.name)))))
            .collect();
        let mut out = String::new();
        if fields.iter().any(|f| !py_ident(&f.name)) {
            // A key that isn't a Python name needs the other spelling,
            // which can't mark single keys as optional.
            let total = fields.iter().all(|f| f.required);
            let _ = writeln!(out, "{name} = TypedDict(\n    \"{name}\",\n    {{");
            for (f, e) in &typed {
                let _ = writeln!(out, "        {}: {e},", Value::from(f.name.as_str()));
            }
            let _ = writeln!(
                out,
                "    }},{}\n)",
                if total { "" } else { "\n    total=False," }
            );
            self.defs.push(out);
            return;
        }
        let required: Vec<&(&Field, String)> = typed.iter().filter(|(f, _)| f.required).collect();
        let optional: Vec<&(&Field, String)> = typed.iter().filter(|(f, _)| !f.required).collect();
        let body = |list: &[&(&Field, String)]| -> String {
            if list.is_empty() {
                return "    pass\n".into();
            }
            list.iter()
                .map(|(f, e)| format!("    {}: {e}\n", f.name))
                .collect()
        };
        match (required.is_empty(), optional.is_empty()) {
            (_, true) => {
                let _ = write!(out, "class {name}(TypedDict):\n{}", body(&required));
            }
            (true, false) => {
                let _ = write!(
                    out,
                    "class {name}(TypedDict, total=False):\n{}",
                    body(&optional)
                );
            }
            (false, false) => {
                let _ = write!(
                    out,
                    "class _{name}Required(TypedDict):\n{}\n\nclass {name}(_{name}Required, total=False):\n{}",
                    body(&required),
                    body(&optional)
                );
            }
        }
        self.defs.push(out);
    }

    /// A named type: a class for an object, an alias for anything else.
    fn named(&mut self, t: &Ty, name: &str, comment: &str) -> String {
        let e = self.expr(t, name);
        if e != name {
            self.defs.push(format!("#: {comment}\n{name} = {e}\n"));
        }
        name.to_string()
    }
}

fn python(units: &[Unit], calls: &[Callable], from: &str) -> String {
    let mut py = Py::default();
    for c in calls {
        let base = pascal(&c.name);
        if let Some(t) = &c.input {
            let what = format!("What the function {} takes.", c.name);
            py.named(t, &format!("{base}Input"), &what);
        }
        if let Some(t) = &c.output {
            let what = format!("What the function {} answers with.", c.name);
            py.named(t, &format!("{base}Output"), &what);
        }
    }
    for unit in units {
        for (suffix, titles, shape) in &unit.shapes {
            let base = format!("{}{suffix}", unit.name);
            let routes = routes_text(titles);
            let params = match &shape.params {
                Some(p) => py.named(
                    p,
                    &format!("{base}Params"),
                    &format!("The path parameters of {routes}."),
                ),
                None => "dict[str, Any]".into(),
            };
            let query = match &shape.query {
                Some(q) => py.named(
                    q,
                    &format!("{base}Query"),
                    &format!("The query parameters of {routes}."),
                ),
                None => "dict[str, Any]".into(),
            };
            let body = match &shape.body {
                Some(b) => {
                    let name =
                        py.named(b, &format!("{base}Body"), &format!("The body of {routes}."));
                    if shape.body_optional {
                        format!("{name} | None")
                    } else {
                        name
                    }
                }
                None => "Any".into(),
            };
            if let Some(r) = &shape.response {
                py.named(
                    r,
                    &format!("{base}Response"),
                    &format!("What {routes} answers with."),
                );
            }
            py.defs.push(format!(
                "#: {routes}\n{base}Request = Request[{params}, {query}, {body}]\n"
            ));
        }
    }
    let mut out = format!(
        "# Generated by `unsworn gen python` from {from}. Don't edit it: change the\n\
         # config, or a schema it names, and generate it again.\n\n\
         from __future__ import annotations\n\n\
         from typing import Any, Literal, TypedDict  # noqa: F401\n\n\
         from unsworn import Request  # noqa: F401\n"
    );
    for def in &py.defs {
        let _ = write!(out, "\n\n{def}");
    }
    out
}

// ------------------------------------------------------------------------ Go

/// `user_id` as `UserID`: Go spells common initialisms in capitals.
fn go_field(name: &str) -> String {
    const INITIALISMS: &[&str] = &[
        "id", "url", "uri", "api", "ip", "http", "https", "json", "uuid",
    ];
    let mut out = String::new();
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        if word.is_empty() {
            return;
        }
        if INITIALISMS.contains(&word.to_ascii_lowercase().as_str()) {
            out.push_str(&word.to_ascii_uppercase());
        } else {
            let mut chars = word.chars();
            if let Some(first) = chars.next() {
                out.push(first.to_ascii_uppercase());
                out.push_str(chars.as_str());
            }
        }
        word.clear();
    };
    let mut prev_lower = false;
    for c in name.chars() {
        if !c.is_ascii_alphanumeric() {
            flush(&mut word, &mut out);
            prev_lower = false;
            continue;
        }
        if c.is_ascii_uppercase() && prev_lower {
            flush(&mut word, &mut out);
        }
        prev_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
        word.push(c);
    }
    flush(&mut word, &mut out);
    if out.chars().next().is_none_or(|c| c.is_ascii_digit()) {
        out.insert(0, 'F');
    }
    out
}

/// For a language without unions: the one type every member of a union
/// comes down to, and whether `null` is among them. `"a" | "b"` is a
/// string, and `string | null` a string that may be missing.
fn narrowed(parts: &[Ty], mut expr: impl FnMut(&Ty) -> String) -> Option<(String, bool)> {
    let nullable = parts.contains(&Ty::Null);
    let mut found: Option<String> = None;
    for p in parts.iter().filter(|p| **p != Ty::Null) {
        let e = expr(p);
        match &found {
            None => found = Some(e),
            Some(first) if *first == e => {}
            Some(_) => return None,
        }
    }
    found.map(|e| (e, nullable))
}

/// Whether a Go type already has `nil` for "not there".
fn go_nilable(e: &str) -> bool {
    e.starts_with("[]") || e.starts_with("map[") || e.starts_with('*') || e == "any"
}

#[derive(Default)]
struct Go {
    defs: Vec<String>,
}

impl Go {
    fn expr(&mut self, t: &Ty, name: &str) -> String {
        match t {
            Ty::Union(parts) => match narrowed(parts, |p| self.expr(p, name)) {
                Some((e, true)) if !go_nilable(&e) => format!("*{e}"),
                Some((e, _)) => e,
                None => "any".into(),
            },
            Ty::Any | Ty::Null | Ty::All(_) => "any".into(),
            Ty::Bool | Ty::Lit(Value::Bool(_)) => "bool".into(),
            Ty::Int => "int64".into(),
            Ty::Num => "float64".into(),
            Ty::Str | Ty::Lit(Value::String(_)) => "string".into(),
            Ty::Lit(Value::Number(n)) if n.is_i64() || n.is_u64() => "int64".into(),
            Ty::Lit(Value::Number(_)) => "float64".into(),
            Ty::Lit(_) => "any".into(),
            Ty::Array(inner) => format!("[]{}", self.expr(inner, &format!("{name}Item"))),
            Ty::Tuple(_) => "[]any".into(),
            Ty::Map(inner) => format!("map[string]{}", self.expr(inner, &format!("{name}Value"))),
            Ty::Object(fields) => {
                self.strukt(name, fields, "");
                name.to_string()
            }
        }
    }

    fn strukt(&mut self, name: &str, fields: &[Field], comment: &str) {
        let mut out = String::new();
        if !comment.is_empty() {
            let _ = writeln!(out, "// {name} is {comment}");
        }
        let _ = writeln!(out, "type {name} struct {{");
        let mut used: Vec<String> = Vec::new();
        let mut rows: Vec<(String, String, String)> = Vec::new();
        for f in fields {
            let mut field = go_field(&f.name);
            while used.contains(&field) {
                field.push('_');
            }
            used.push(field.clone());
            let e = self.expr(&f.ty, &format!("{name}{field}"));
            let tag = Value::from(f.name.as_str()).to_string();
            let tag = tag.trim_matches('"');
            // A missing optional value is nil, not the type's zero.
            let (e, tag) = if f.required {
                (e, tag.to_string())
            } else if go_nilable(&e) {
                (e, format!("{tag},omitempty"))
            } else {
                (format!("*{e}"), format!("{tag},omitempty"))
            };
            rows.push((field, e, tag));
        }
        // In columns, as gofmt would leave them.
        let names = rows.iter().map(|r| r.0.len()).max().unwrap_or(0);
        let types = rows.iter().map(|r| r.1.len()).max().unwrap_or(0);
        for (field, e, tag) in rows {
            let _ = writeln!(out, "\t{field:<names$} {e:<types$} `json:\"{tag}\"`");
        }
        out.push_str("}\n");
        self.defs.push(out);
    }

    fn named(&mut self, t: &Ty, name: &str, comment: &str) {
        match t {
            Ty::Object(fields) => self.strukt(name, fields, comment),
            other => {
                let e = self.expr(other, name);
                self.defs
                    .push(format!("// {name} is {comment}\ntype {name} = {e}\n"));
            }
        }
    }
}

fn go(units: &[Unit], calls: &[Callable], from: &str, package: &str) -> String {
    let mut go = Go::default();
    for c in calls {
        let base = pascal(&c.name);
        if let Some(t) = &c.input {
            let what = format!("what the function {} takes. Pass it to req.Call.", c.name);
            go.named(t, &format!("{base}Input"), &what);
        }
        if let Some(t) = &c.output {
            let what = format!("what the function {} answers with.", c.name);
            go.named(t, &format!("{base}Output"), &what);
        }
    }
    for unit in units {
        for (suffix, titles, shape) in &unit.shapes {
            let base = format!("{}{suffix}", unit.name);
            let routes = routes_text(titles);
            let parts = [
                (
                    &shape.params,
                    "Params",
                    "the path parameters of",
                    "req.BindParams",
                ),
                (
                    &shape.query,
                    "Query",
                    "the query parameters of",
                    "req.BindQuery",
                ),
                (&shape.body, "Body", "the body of", "req.Bind"),
                (
                    &shape.response,
                    "Response",
                    "what the handler answers with for",
                    "",
                ),
            ];
            for (t, kind, what, bind) in parts {
                let Some(t) = t else { continue };
                let read = if bind.is_empty() {
                    String::new()
                } else {
                    format!(" Read it with {bind}.")
                };
                go.named(
                    t,
                    &format!("{base}{kind}"),
                    &format!("{what} {routes}.{read}"),
                );
            }
        }
    }
    let mut out = format!(
        "// Code generated by `unsworn gen go` from {from}. DO NOT EDIT.\n\
         // Change the config, or a schema it names, and generate it again.\n\n\
         package {package}\n"
    );
    for def in &go.defs {
        let _ = write!(out, "\n{def}");
    }
    out
}

// ---------------------------------------------------------------------- Rust

const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "dyn", "else", "enum", "extern", "false",
    "fn", "for", "gen", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "static", "struct", "trait", "true", "try", "type", "unsafe", "use", "where",
    "while", "abstract", "become", "box", "do", "final", "macro", "override", "priv", "typeof",
    "unsized", "virtual", "yield",
];

/// `userId` and `user-id` as `user_id`.
fn snake(name: &str) -> String {
    let mut out = String::new();
    let mut prev_lower = false;
    for c in name.chars() {
        if !c.is_ascii_alphanumeric() {
            if !out.ends_with('_') && !out.is_empty() {
                out.push('_');
            }
            prev_lower = false;
            continue;
        }
        if c.is_ascii_uppercase() && prev_lower {
            out.push('_');
        }
        prev_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
        out.push(c.to_ascii_lowercase());
    }
    let out = out.trim_end_matches('_').to_string();
    if out.chars().next().is_none_or(|c| c.is_ascii_digit()) {
        return format!("f_{out}");
    }
    out
}

#[derive(Default)]
struct Rs {
    defs: Vec<String>,
}

impl Rs {
    fn expr(&mut self, t: &Ty, name: &str) -> String {
        match t {
            Ty::Union(parts) => match narrowed(parts, |p| self.expr(p, name)) {
                Some((e, true)) if !e.starts_with("Option<") => format!("Option<{e}>"),
                Some((e, _)) => e,
                None => "serde_json::Value".into(),
            },
            Ty::Any | Ty::Null | Ty::All(_) => "serde_json::Value".into(),
            Ty::Bool | Ty::Lit(Value::Bool(_)) => "bool".into(),
            Ty::Int => "i64".into(),
            Ty::Num => "f64".into(),
            Ty::Str | Ty::Lit(Value::String(_)) => "String".into(),
            Ty::Lit(Value::Number(n)) if n.is_i64() || n.is_u64() => "i64".into(),
            Ty::Lit(Value::Number(_)) => "f64".into(),
            Ty::Lit(_) => "serde_json::Value".into(),
            Ty::Array(inner) => format!("Vec<{}>", self.expr(inner, &format!("{name}Item"))),
            Ty::Tuple(_) => "Vec<serde_json::Value>".into(),
            Ty::Map(inner) => format!(
                "std::collections::BTreeMap<String, {}>",
                self.expr(inner, &format!("{name}Value"))
            ),
            Ty::Object(fields) => {
                self.strukt(name, fields, "");
                name.to_string()
            }
        }
    }

    fn strukt(&mut self, name: &str, fields: &[Field], comment: &str) {
        let mut out = String::new();
        if !comment.is_empty() {
            let _ = writeln!(out, "/// {comment}");
        }
        let _ = writeln!(
            out,
            "#[derive(Debug, Clone, Deserialize, Serialize)]\npub struct {name} {{"
        );
        let mut used: Vec<String> = Vec::new();
        for f in fields {
            let mut field = snake(&f.name);
            while used.contains(&field) {
                field.push('_');
            }
            used.push(field.clone());
            let e = self.expr(&f.ty, &format!("{name}{}", pascal(&f.name)));
            if field != f.name {
                let _ = writeln!(
                    out,
                    "    #[serde(rename = {})]",
                    Value::from(f.name.as_str())
                );
            }
            let ident = if RUST_KEYWORDS.contains(&field.as_str()) {
                format!("r#{field}")
            } else {
                field
            };
            if f.required {
                let _ = writeln!(out, "    pub {ident}: {e},");
            } else {
                let e = match e.starts_with("Option<") {
                    true => e,
                    false => format!("Option<{e}>"),
                };
                let _ = writeln!(
                    out,
                    "    #[serde(default, skip_serializing_if = \"Option::is_none\")]\n    pub {ident}: {e},"
                );
            }
        }
        out.push_str("}\n");
        self.defs.push(out);
    }

    fn named(&mut self, t: &Ty, name: &str, comment: &str) {
        match t {
            Ty::Object(fields) => self.strukt(name, fields, comment),
            other => {
                let e = self.expr(other, name);
                self.defs
                    .push(format!("/// {comment}\npub type {name} = {e};\n"));
            }
        }
    }
}

fn rust(units: &[Unit], calls: &[Callable], from: &str) -> String {
    let mut rs = Rs::default();
    for c in calls {
        let base = pascal(&c.name);
        if let Some(t) = &c.input {
            let what = format!("What the function {} takes. Pass it to `req.call`.", c.name);
            rs.named(t, &format!("{base}Input"), &what);
        }
        if let Some(t) = &c.output {
            let what = format!("What the function {} answers with.", c.name);
            rs.named(t, &format!("{base}Output"), &what);
        }
    }
    for unit in units {
        for (suffix, titles, shape) in &unit.shapes {
            let base = format!("{}{suffix}", unit.name);
            let routes = routes_text(titles);
            let parts = [
                (
                    &shape.params,
                    "Params",
                    "The path parameters of",
                    "req.params()",
                ),
                (
                    &shape.query,
                    "Query",
                    "The query parameters of",
                    "req.query()",
                ),
                (&shape.body, "Body", "The body of", "req.body_json()"),
                (
                    &shape.response,
                    "Response",
                    "What the handler answers with for",
                    "",
                ),
            ];
            for (t, kind, what, read) in parts {
                let Some(t) = t else { continue };
                let read = if read.is_empty() {
                    String::new()
                } else {
                    format!(" Read it with `{read}`.")
                };
                rs.named(
                    t,
                    &format!("{base}{kind}"),
                    &format!("{what} {routes}.{read}"),
                );
            }
        }
    }
    let mut out = format!(
        "// Generated by `unsworn gen rust` from {from}. Don't edit it: change the\n\
         // config, or a schema it names, and generate it again.\n\n\
         #![allow(dead_code)]\n\n\
         use serde::{{Deserialize, Serialize}};\n"
    );
    for def in &rs.defs {
        let _ = write!(out, "\n{def}");
    }
    out
}

#[cfg(test)]
mod tests;
