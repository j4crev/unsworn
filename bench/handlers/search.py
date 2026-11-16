"""The handler bench/run.py's "work" group calls. See search.ts."""

from unsworn import json

SYLLABLES = [
    "al", "be", "cor", "del", "en", "fa", "gra", "hy", "in", "jo", "ka", "lu", "mor",
    "ne", "or", "pra", "qui", "ro", "sta", "tel", "ur", "ve", "wil", "xa", "yo", "zen",
]  # fmt: skip


def make_names():
    seed = 42
    names = []
    for _ in range(2000):
        seed = seed * 48271 % 2147483647
        name = ""
        for _ in range(3 + seed % 4):
            seed = seed * 48271 % 2147483647
            name += SYLLABLES[seed % len(SYLLABLES)]
        names.append(name)
    return names


NAMES = make_names()


def distance(a, b, row):
    """The edit distance between two strings, keeping one row of the table."""
    for j in range(len(b) + 1):
        row[j] = j
    for i in range(1, len(a) + 1):
        diagonal = row[0]
        row[0] = i
        ca = a[i - 1]
        for j in range(1, len(b) + 1):
            above = row[j]
            row[j] = diagonal if ca == b[j - 1] else 1 + min(diagonal, above, row[j - 1])
            diagonal = above
    return row[len(b)]


def search(req, ctx):
    q = str(req.query["q"])
    row = [0] * (len(q) + 1)
    best = []  # (index, distance), closest first
    for index, name in enumerate(NAMES):
        d = distance(name, q, row)
        if len(best) == 5 and d >= best[4][1]:
            continue
        at = len(best)
        while at > 0 and best[at - 1][1] > d:
            at -= 1
        best.insert(at, (index, d))
        del best[5:]
    return json({"query": q, "matches": [{"name": NAMES[i], "distance": d} for i, d in best]})
