# WebAssembly handlers: a prototype to measure them

The roadmap has handlers that run as WebAssembly inside the server's
process, instead of in a worker process. Before building that, this
measures the one thing it depends on: what a call into a WebAssembly
handler costs, against the same code run natively.

It isn't part of Unsworn. `guest` is a handler module (the "hello"
handler, and the search handler from `bench/handlers/search.rs`) that
takes one protocol `request` message as JSON and gives back one
`response`. `host` loads it with [wasmtime](https://wasmtime.dev) and
times calls.

```sh
cd bench/wasm-prototype
(cd guest && cargo build --release --target wasm32-unknown-unknown)
(cd host && cargo build --release)
taskset -c 2 host/target/release/host guest/target/wasm32-unknown-unknown/release/guest.wasm
```

## What it measured

On the benchmark machine (an Intel i5-1145G7), on one core, wasmtime
50.0.2, the best of five runs, three times over with the same result:

| | Time a call |
|---|--:|
| "hello", called natively (parse the request, answer, serialize) | 1.63 µs |
| "hello", as WebAssembly, one instance kept | 2.31 µs |
| "hello", as WebAssembly, a new instance for each call | 19.8 µs |
| Search, called natively | 419 µs |
| Search, as WebAssembly | 869 µs |

- **The call itself is cheap.** Crossing into WebAssembly and back, with
  the request and response copied through the module's memory, adds
  0.7 µs. A call to a handler in a worker process adds about 20 µs of CPU
  today (a Bun "hello" at 42.6 µs a request against 23 µs for a static
  answer, in `docs/benchmarks.md`).
- **A fresh instance for each call costs 18 µs more,** so instances would
  be kept and reused, as workers are.
- **Code that computes runs half as fast.** The search handler takes
  twice as long as WebAssembly as it does natively. So for an endpoint
  that's moved to Rust because it computes, a native worker is still the
  faster place: its 20 µs hop is small next to 400.
- **Compiling the module takes 59 ms** (103 KB of WebAssembly), once, at
  startup. That can be done ahead of time.
- **wasmtime is a large dependency:** 173 packages in this prototype's
  lockfile, and a 16 MB program before any of Unsworn is in it.

A single-core loop like this one understates what a call costs on a
loaded server, by about two and a half times on this machine in earlier
comparisons, so read the ratios, not the microseconds.

## What follows from it

WebAssembly handlers would suit handlers that do little, where the hop is
most of the cost, and they would run with no worker process and nothing
but what Unsworn hands them. They wouldn't suit the handler that needs
raw speed, which is the case the README makes for Rust.

One more thing turned up: `usize` is 32 bits in WebAssembly. The search
handler's name generator multiplies in `usize`, which overflowed and gave
other names until the prototype did that sum in `u64`. A handler moved
from a worker to WebAssembly can change its answers that way.
