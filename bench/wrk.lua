-- Used by run.py: sends BENCH_METHOD with the body in BENCH_BODY_FILE, if
-- set, and prints the results as one JSON line.
local method = os.getenv("BENCH_METHOD")
local body_file = os.getenv("BENCH_BODY_FILE")
if method then wrk.method = method end
if body_file and body_file ~= "" then
  local f = assert(io.open(body_file, "rb"))
  wrk.body = f:read("*a")
  f:close()
end

function done(summary, latency, requests)
  local e = summary.errors
  io.write(string.format(
    '\nBENCH_JSON {"requests":%d,"duration_us":%d,"bytes":%d,"non2xx":%d,' ..
    '"errors":%d,"p50_us":%d,"p90_us":%d,"p99_us":%d,"p999_us":%d,"max_us":%d,"mean_us":%.1f}\n',
    summary.requests, summary.duration, summary.bytes, e.status,
    e.connect + e.read + e.write + e.timeout,
    latency:percentile(50), latency:percentile(90), latency:percentile(99),
    latency:percentile(99.9), latency.max, latency.mean))
end
