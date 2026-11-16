// The handler bench/run.py's "work" group calls: a fuzzy search over 2,000
// product names held in memory. It answers with the five names closest to
// `?q=` by edit distance. search.py, search.go and search.rs are the same
// handler, written the same way, so the group compares what the languages
// make of one piece of ordinary code. See docs/benchmarks.md.

import type { Handler } from "../../sdk/js/src/index.ts";

const SYLLABLES = [
  "al", "be", "cor", "del", "en", "fa", "gra", "hy", "in", "jo", "ka", "lu", "mor",
  "ne", "or", "pra", "qui", "ro", "sta", "tel", "ur", "ve", "wil", "xa", "yo", "zen",
];

// The same 2,000 names in every language: three to six syllables each,
// picked by a small random number generator with a fixed seed.
function makeNames(): string[] {
  let seed = 42;
  const next = () => (seed = (seed * 48271) % 2147483647);
  const names: string[] = [];
  for (let i = 0; i < 2000; i++) {
    let name = "";
    for (let n = 3 + (next() % 4); n > 0; n--) name += SYLLABLES[next() % SYLLABLES.length];
    names.push(name);
  }
  return names;
}

const NAMES = makeNames();

// The edit distance between two strings, keeping one row of the table.
function distance(a: string, b: string, row: number[]): number {
  for (let j = 0; j <= b.length; j++) row[j] = j;
  for (let i = 1; i <= a.length; i++) {
    let diagonal = row[0];
    row[0] = i;
    const ca = a.charCodeAt(i - 1);
    for (let j = 1; j <= b.length; j++) {
      const above = row[j];
      row[j] = ca === b.charCodeAt(j - 1) ? diagonal : 1 + Math.min(diagonal, above, row[j - 1]);
      diagonal = above;
    }
  }
  return row[b.length];
}

export const search: Handler = (req) => {
  const q = String(req.query.q);
  const row: number[] = new Array(q.length + 1).fill(0);
  const best: { index: number; distance: number }[] = []; // closest first
  for (let index = 0; index < NAMES.length; index++) {
    const d = distance(NAMES[index], q, row);
    if (best.length === 5 && d >= best[4].distance) continue;
    let at = best.length;
    while (at > 0 && best[at - 1].distance > d) at--;
    best.splice(at, 0, { index, distance: d });
    if (best.length > 5) best.pop();
  }
  return {
    json: { query: q, matches: best.map((m) => ({ name: NAMES[m.index], distance: m.distance })) },
  };
};
