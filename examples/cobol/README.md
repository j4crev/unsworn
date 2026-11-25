# COBOL example

A loan calculator whose handlers, and the worker that runs them, are COBOL.
There's no SDK: the worker reads lines of JSON on stdin and writes lines of
JSON on stdout. Needs GnuCOBOL 3.1 or newer for `cobc` (`apt install
gnucobol`, `brew install gnucobol`, or `gnucobol` in the AUR).

```sh
unsworn dev
```

```sh
curl 'localhost:8080/loans/payment?principal=250000&rate=6.5&months=360'
curl 'localhost:8080/loans/schedule?principal=1200.50&rate=5&months=3'
curl 'localhost:8080/loans/afford?payment=2000&rate=6.5&months=360'
curl 'localhost:8080/loans/payment?principal=-1&rate=6.5&months=0'    # 400, and COBOL never sees it
```

```json
{"payment":1580.17,"last_payment":1580.55,"payments":360,"total_paid":568861.58,"total_interest":318861.58}
```

`rate` is percent a year. Every month is rounded to the cent, and the last
payment takes up what that leaves over.

| File | |
|---|---|
| [`src/loans.cbl`](src/loans.cbl) | The handlers: one program each, and the arithmetic |
| [`src/worker.cbl`](src/worker.cbl) | The loop that reads requests, calls a handler and writes its response |
| [`src/json.cbl`](src/json.cbl) | Looks up a member of a message. GnuCOBOL can't parse JSON |
| [`unsworn.toml`](unsworn.toml) | The routes and their rules, and how to build and run the worker |

COBOL suits this. Its numbers are decimal, so cents are exact, and
`FUNCTION ANNUITY` works out a loan payment. What COBOL is bad at, such as
HTTP and checking what callers send, isn't in COBOL: the patterns in
`unsworn.toml` are why a handler can trust that an amount fits its field.

A worker trusts the messages it's sent, so one written by hand has to keep
what callers write from being read as part of a message (see
[reading lines safely](../../docs/protocol.md#reading-lines-safely)).
In COBOL that takes some care:

- `LINE SEQUENTIAL` files have a longest line, and what's past it may be
  read as a line of its own. The worker's is 2 MB, far more than a request
  without a body normally takes, and the worker stops at a line that
  reaches it.
- Comparisons ignore trailing spaces, so a query parameter named
  `principal ` (with a space), which no rule covers, could pass for
  `principal`. The JSON reader compares lengths too.
- The JSON reader takes strings whole, so text in a header that looks like
  JSON isn't mistaken for it. It also follows nesting, so a `rate` in some
  other object isn't mistaken for the one in `query`.

The worker does only what these routes need. To give it requests with a
body, raise `max_body` and make the records in `src/worker.cbl` and
`src/copy/message.cpy` large enough to match. For JSON, `src/json.cbl` also
has to allow spaces between tokens and decode escapes. For anything else,
the worker has to read the raw bytes after a message that has `body_len`
and say `raw_body` in `ready`.
