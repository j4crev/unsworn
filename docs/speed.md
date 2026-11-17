## Proxy vs nginx

A static answer already costs 23 µs, the same as nginx's whole proxied request, and the upstream leg adds another 36 µs. So both halves need work. Your earlier profiles were flat, so I'd expect several medium wins, not one hot spot.

1. **Go thread-per-core for the proxy path.** Today one work-stealing tokio runtime serves everything, and the upstream client is hyper-util's generic pooled `Client` ([support.rs:155](/home/j4crev/Dev/unsworn/crates/unsworn/src/server/support.rs:155)). Each proxied request crosses from the client's connection task to a separate upstream connection task and back, through a pool behind a shared lock, often waking another thread. nginx does both sockets on one thread with no locks. The equivalent is one single-threaded runtime per core with `SO_REUSEPORT`, a per-thread upstream pool, and hyper's low-level `client::conn::http1` used directly. This is the biggest candidate.
2. **Cut the fixed cost every request pays.** This helps static routes too.
   - Metrics take a global mutex and allocate two strings per request ([support.rs:179](/home/j4crev/Dev/unsworn/crates/unsworn/src/server/support.rs:179)); pre-registered per-route atomic counters would remove both.
   - The access log costs about 4 µs; a cached timestamp and a batched writer would cut it.
   - `rewrite` formats the upstream URL into a string and re-parses it on every request; the upstream could be parsed once at load. This is small, about 1 µs.
3. **Support Unix-socket upstreams.** With the app on the same host, which is the common deployment, that skips the loopback TCP stack, where your profile put about 28% of the time.
4. **io_uring**, to batch the sends and receives. This is the largest refactor and the most speculative, so I'd leave it last.

Before any of this, I'd build a bare 100-line hyper proxy (per-core runtime, low-level client, no policies) and run it through the same harness. If it lands near 30 µs, the gap is Unsworn's architecture and items 1–2 will close it. If it lands near 50 µs, the gap is hyper and tokio themselves, and only item 4 or a different HTTP stack would.

## Large JSON bodies

Yes, raw bytes are an option: that is already how every non-JSON body reaches a handler, as bytes after the message. A JSON body takes the expensive path only because the protocol puts it inside the message's `json` member ([flow.rs:497](/home/j4crev/Dev/unsworn/crates/unsworn-core/src/pipeline/flow.rs:497)). That placement costs three things:

- **Unsworn must check every byte.** The text is spliced into the message unchecked, so invalid JSON would corrupt it.
- **The body is copied again**, and once more if it contains a newline (any pretty-printed body).
- **The worker parses it whether or not the handler reads it.** The JS SDK runs `JSON.parse` on the whole message ([protocol.ts:238](/home/j4crev/Dev/unsworn/sdk/js/src/protocol.ts:238)), and Go's decoder scans all 64 KB just to find where it ends. The benchmark handler answers `204` without touching the body.

The refactor would be:

1. **Send a JSON body as the raw body section**, with a flag saying it is JSON, and have each SDK parse it lazily on first access to `req.json`. Workers would advertise support in `ready`, as they do for `stream_body`, so old SDKs keep working.
2. **Drop Unsworn's check when the route has no body schema.** Only the worker's parser ever reads the body then, so the duplicate-name concern doesn't apply. The catch is that malformed JSON would be rejected by the SDK instead of by Unsworn, so the SDKs need to produce the same `400`.
3. **Do the same for responses.** The SDK serialises once into the raw section and Unsworn passes it through unchecked.
4. **For routes with a body schema, use a faster parser.** The document still has to be built there; a SIMD parser (simd-json or sonic-rs) is the straightforward win, and validating against the schema without building the document is the ambitious one.

A handler that ignores the body should then cost about what raw bytes do (roughly 0.25–0.32 ms against 1.2–1.9 ms today). One that reads it pays that plus a single parse in its own language.