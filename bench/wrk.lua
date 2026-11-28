-- Used by run.py: sends BENCH_METHOD with the body in BENCH_BODY_FILE, if
-- set, and prints the results as one JSON line.
-- If BENCH_UNIQUE_ID_TOKEN is set, each request replaces that token in the
-- body with a unique per-thread monotonic integer string (for at-most-once
-- transactional write benchmarks such as [dimedb]).
local method = os.getenv("BENCH_METHOD")
local body_file = os.getenv("BENCH_BODY_FILE")
local unique_token = os.getenv("BENCH_UNIQUE_ID_TOKEN")
local body_template = nil

if method then wrk.method = method end
if body_file and body_file ~= "" then
  local f = assert(io.open(body_file, "rb"))
  local content = f:read("*a")
  f:close()
  if unique_token and unique_token ~= "" then
    body_template = content
  else
    wrk.body = content
  end
end

local thread_counter = 0
function setup(thread)
  thread_counter = thread_counter + 1
  thread:set("tid", thread_counter)
end

local req_seq = 0
local id_prefix = 0
function init(args)
  -- Offset by time so warmup and measured runs never reuse request_ids.
  id_prefix = (os.time() % 1000000) * 1000000000 + tid * 10000000
end

if unique_token and unique_token ~= "" then
  function request()
    req_seq = req_seq + 1
    local body = string.gsub(body_template, unique_token, tostring(id_prefix + req_seq))
    return wrk.format(nil, nil, nil, body)
  end
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
