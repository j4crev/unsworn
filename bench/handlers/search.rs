//! The handler bench/run.py's "work" group calls. See search.ts.

use std::sync::LazyLock;

use unsworn_sdk::{Reply, Request, Result, handlers, json};

const SYLLABLES: [&str; 26] = [
    "al", "be", "cor", "del", "en", "fa", "gra", "hy", "in", "jo", "ka", "lu", "mor", "ne", "or",
    "pra", "qui", "ro", "sta", "tel", "ur", "ve", "wil", "xa", "yo", "zen",
];

static NAMES: LazyLock<Vec<String>> = LazyLock::new(make_names);

fn main() {
    LazyLock::force(&NAMES);
    unsworn_sdk::run(handlers![search]);
}

fn make_names() -> Vec<String> {
    let mut seed: usize = 42;
    let mut next = || {
        seed = seed * 48271 % 2147483647;
        seed
    };
    (0..2000)
        .map(|_| {
            let mut name = String::new();
            for _ in 0..3 + next() % 4 {
                name += SYLLABLES[next() % SYLLABLES.len()];
            }
            name
        })
        .collect()
}

/// The edit distance between two strings, keeping one row of the table.
fn distance(a: &[u8], b: &[u8], row: &mut [usize]) -> usize {
    for (j, cell) in row.iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        let mut diagonal = row[0];
        row[0] = i;
        let ca = a[i - 1];
        for j in 1..=b.len() {
            let above = row[j];
            row[j] = if ca == b[j - 1] {
                diagonal
            } else {
                1 + diagonal.min(above).min(row[j - 1])
            };
            diagonal = above;
        }
    }
    row[b.len()]
}

async fn search(req: Request) -> Result {
    let q: String = req.query_value("q")?;
    let mut row = vec![0; q.len() + 1];
    let mut best: Vec<(usize, usize)> = Vec::with_capacity(6); // (index, distance), closest first
    for (index, name) in NAMES.iter().enumerate() {
        let d = distance(name.as_bytes(), q.as_bytes(), &mut row);
        if best.len() == 5 && d >= best[4].1 {
            continue;
        }
        let at = best.iter().rposition(|m| m.1 <= d).map_or(0, |at| at + 1);
        best.insert(at, (index, d));
        best.truncate(5);
    }
    let matches: Vec<_> = best
        .iter()
        .map(|&(index, distance)| json!({ "name": NAMES[index], "distance": distance }))
        .collect();
    Ok(Reply::json(json!({ "query": q, "matches": matches })))
}
