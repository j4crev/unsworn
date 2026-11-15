// Where a JS host keeps rate limits and cached responses when the platform
// has no store of its own: Redis behind a REST API, as `[store]` says, or
// this process's memory.

import { redisRateLimitScript, type StoreSettings, takeToken } from "../wasm/unsworn.js";

/** The store calls of a `Bridge`. */
export interface Store {
  /** `null` when allowed, or milliseconds until a token is available. */
  rateLimit(key: string, requests: number, perMs: number): Promise<number | null>;
  cacheGet(key: string): Promise<Uint8Array | null>;
  cachePut(key: string, value: Uint8Array, ttlMs: number): Promise<void>;
}

/** The store `[store]` asks for. Throws for one this host can't reach. */
export function storeFor(settings: StoreSettings): Store {
  if (settings.backend === "memory") return memoryStore();
  const url = settings.url ?? "";
  if (!/^https?:\/\//.test(url)) {
    throw new Error(
      "[store].url: functions reach Redis through a REST API, so give an https:// URL and a token (redis:// is for the server)",
    );
  }
  return redisRestStore(url, settings.token ?? "", settings.prefix);
}

/** How long a REST call may take. The core gives up on the store after 1s. */
const REST_TIMEOUT_MS = 2000;

/**
 * Redis through a REST API that speaks Upstash's protocol: a command is
 * POSTed as a JSON array, and the answer is `{ result }` or `{ error }`.
 *
 * Rate limits run the same script on the same keys as the server's Redis
 * store, so a server and a function can share buckets. Cached responses
 * can't be shared with it: JSON can't carry bytes, so they're stored as
 * base64 under keys of their own.
 */
export function redisRestStore(url: string, token: string, prefix = ""): Store {
  const endpoint = url.replace(/\/+$/, "");
  let script: string | undefined;

  async function command(args: (string | number)[]): Promise<unknown> {
    const res = await fetch(endpoint, {
      method: "POST",
      headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
      body: JSON.stringify(args),
      signal: AbortSignal.timeout(REST_TIMEOUT_MS),
    });
    const data = (await res.json().catch(() => null)) as { result?: unknown; error?: string } | null;
    if (!res.ok || !data || data.error) throw new Error(data?.error ?? `the Redis REST API answered ${res.status}`);
    return data.result;
  }

  return {
    async rateLimit(key, requests, perMs) {
      script ??= redisRateLimitScript();
      const wait = Number(await command(["EVAL", script, 1, `${prefix}rl:${key}`, requests, perMs]));
      if (!Number.isFinite(wait)) throw new Error("the rate limit script gave no number");
      return wait > 0 ? wait : null;
    },

    async cacheGet(key) {
      const value = await command(["GET", `${prefix}cache64:${key}`]);
      return typeof value === "string" ? new Uint8Array(Buffer.from(value, "base64")) : null;
    },

    async cachePut(key, value, ttlMs) {
      const text = Buffer.from(value).toString("base64");
      await command(["SET", `${prefix}cache64:${key}`, text, "PX", Math.max(1, Math.ceil(ttlMs))]);
    },
  };
}

/** As many cached responses, and bytes of them, as the server's memory store keeps. */
const MAX_ENTRIES = 10_000;
const MAX_BYTES = 64 * 1024 * 1024;
/** Remembered API-key verdicts get a space of their own, so made-up keys can't push responses out. */
const MAX_VERDICTS = 10_000;
const MAX_VERDICT_BYTES = 8 * 1024 * 1024;
const MAX_BUCKETS = 200_000;

/** Entries within a count and a byte budget; the oldest written go first. */
function boundedCache(maxEntries: number, maxBytes: number) {
  const entries = new Map<string, { value: Uint8Array; until: number; size: number }>();
  let bytes = 0;
  const remove = (key: string) => {
    const entry = entries.get(key);
    if (!entry) return;
    entries.delete(key);
    bytes -= entry.size;
  };
  return {
    get(key: string): Uint8Array | null {
      const entry = entries.get(key);
      if (!entry) return null;
      if (entry.until > Date.now()) return entry.value;
      remove(key);
      return null;
    },
    put(key: string, value: Uint8Array, ttlMs: number) {
      remove(key);
      const size = key.length * 2 + value.byteLength + 128;
      if (size > maxBytes) return;
      while (entries.size >= maxEntries || bytes + size > maxBytes) {
        const oldest = entries.keys().next().value as string | undefined;
        if (oldest === undefined) break;
        remove(oldest);
      }
      entries.set(key, { value, until: Date.now() + ttlMs, size });
      bytes += size;
    },
  };
}

/**
 * This process's memory. Each instance of a function has its own, so rate
 * limits aren't shared and `unsworn check --target` rejects them; it's here
 * for what works anyway, such as remembered API-key verdicts.
 */
export function memoryStore(): Store {
  const buckets = new Map<string, [number, number]>();
  const responses = boundedCache(MAX_ENTRIES, MAX_BYTES);
  const verdicts = boundedCache(MAX_VERDICTS, MAX_VERDICT_BYTES);
  const cacheFor = (key: string) => (key.startsWith("auth|") ? verdicts : responses);
  return {
    async rateLimit(key, requests, perMs) {
      const saved = buckets.get(key);
      const [tokens, lastMs, waitMs] = takeToken(saved?.[0], saved?.[1], Date.now(), requests, perMs);
      buckets.delete(key);
      buckets.set(key, [tokens, lastMs]);
      while (buckets.size > MAX_BUCKETS) buckets.delete(buckets.keys().next().value as string);
      return waitMs < 0 ? null : waitMs;
    },

    async cacheGet(key) {
      return cacheFor(key).get(key);
    },

    async cachePut(key, value, ttlMs) {
      cacheFor(key).put(key, value, ttlMs);
    },
  };
}
