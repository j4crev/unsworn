//! A handler module for the prototype: `hello` and the benchmark's `search`.
use std::sync::LazyLock;

use serde_json::{json, Value};

const SYLLABLES: [&str; 26] = [
    "al", "be", "cor", "del", "en", "fa", "gra", "hy", "in", "jo", "ka", "lu", "mor", "ne", "or",
    "pra", "qui", "ro", "sta", "tel", "ur", "ve", "wil", "xa", "yo", "zen",
];

static NAMES: LazyLock<Vec<String>> = LazyLock::new(|| {
    // 64 bits on purpose: `usize` is 32 bits in WebAssembly, where this
    // multiplication doesn't fit.
    let mut seed: u64 = 42;
    let mut next = || {
        seed = seed * 48271 % 2147483647;
        seed as usize
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
});

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

fn search(q: &str) -> Value {
    let mut row = vec![0; q.len() + 1];
    let mut best: Vec<(usize, usize)> = Vec::with_capacity(6);
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
        .map(|&(i, d)| json!({ "name": NAMES[i], "distance": d }))
        .collect();
    json!({ "query": q, "matches": matches })
}

/// One request message in, one response message out, as the protocol's JSON.
pub fn handle_message(input: &[u8]) -> Vec<u8> {
    let req: Value = serde_json::from_slice(input).unwrap();
    let id = &req["id"];
    let out = match req["handler"].as_str().unwrap_or_default() {
        h if h.ends_with("#search") => {
            json!({ "type": "response", "id": id, "status": 200, "json": search(req["query"]["q"].as_str().unwrap_or_default()) })
        }
        _ => {
            json!({ "type": "response", "id": id, "status": 201, "text": "hello", "headers": { "x-conformance": "yes" } })
        }
    };
    serde_json::to_vec(&out).unwrap()
}

#[no_mangle]
pub extern "C" fn unsworn_alloc(len: usize) -> *mut u8 {
    let mut v = Vec::<u8>::with_capacity(len);
    let p = v.as_mut_ptr();
    std::mem::forget(v);
    p
}

#[no_mangle]
pub unsafe extern "C" fn unsworn_free(ptr: *mut u8, len: usize) {
    drop(Vec::from_raw_parts(ptr, 0, len));
}

#[no_mangle]
pub unsafe extern "C" fn unsworn_handle(ptr: *mut u8, len: usize) -> u64 {
    let input = Vec::from_raw_parts(ptr, len, len);
    let mut out = handle_message(&input);
    out.shrink_to_fit();
    let packed = ((out.as_ptr() as u64) << 32) | out.len() as u64;
    std::mem::forget(out);
    packed
}
