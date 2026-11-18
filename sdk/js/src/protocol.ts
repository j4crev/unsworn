// The Unsworn handler protocol: message types and the two encodings.
// See docs/protocol.md.

export const PROTOCOL_VERSION = 1;

export type Phase = "handle" | "before";

export interface RequestMessage {
  type: "request";
  id: string;
  phase?: Phase;
  handler: string;
  route: string;
  method: string;
  path: string;
  params?: Record<string, unknown>;
  query?: Record<string, unknown>;
  raw_query?: string;
  headers?: Record<string, string>;
  context?: Record<string, unknown>;
  json?: unknown;
  text?: string;
  body_len?: number;
  /** The raw body is the JSON body, in place of `json`. */
  body_json?: boolean;
  stream?: boolean;
  deadline_ms?: number;
}

export interface CancelMessage {
  type: "cancel";
  id: string;
  reason?: string;
}

export interface VerifyMessage {
  type: "verify";
  id: string;
  handlers: string[];
}

/** How a call to a function ended. */
export interface ResultMessage {
  type: "result";
  id: string;
  status: number;
  json?: unknown;
  error?: string;
}

/** A service's answer to a `fetch`. Its body is the raw bytes that follow. */
export interface FetchedMessage {
  type: "fetched";
  id: string;
  status: number;
  headers?: Record<string, string>;
  error?: string;
}

export type Incoming =
  | RequestMessage
  | CancelMessage
  | VerifyMessage
  | ResultMessage
  | FetchedMessage
  | { type: string; id?: string };

export type Outgoing = Record<string, unknown> & { type: string };

/** A message plus the raw body bytes that follow it. */
export interface Frame {
  message: Incoming;
  body?: Uint8Array;
}

const encoder = new TextEncoder();
const decoder = new TextDecoder();

function concat(a: Uint8Array, b: Uint8Array): Uint8Array {
  const out = new Uint8Array(a.length + b.length);
  out.set(a, 0);
  out.set(b, a.length);
  return out;
}

/**
 * Received bytes, kept as the chunks they arrived in. A frame larger than
 * one read used to be joined to everything before it on every read, which
 * copied a 1 MB body about eight times over; here bytes are copied once,
 * and only when what's asked for spans chunks.
 */
class ByteQueue {
  private chunks: Uint8Array[] = [];
  /** The first chunk with unread bytes, and how far into it they start. */
  private first = 0;
  private head = 0;
  length = 0;

  push(chunk: Uint8Array) {
    if (chunk.length === 0) return;
    this.chunks.push(chunk);
    this.length += chunk.length;
  }

  /** The first `n` bytes, without consuming them. `n` is at most `length`. */
  peek(n: number): Uint8Array {
    const first = this.chunks[this.first];
    if (n === 0 || !first) return new Uint8Array(0);
    if (first.length - this.head >= n) return first.subarray(this.head, this.head + n);
    const out = new Uint8Array(n);
    let filled = 0;
    let start = this.head;
    for (let i = this.first; filled < n; i++) {
      const part = this.chunks[i].subarray(start, start + (n - filled));
      out.set(part, filled);
      filled += part.length;
      start = 0;
    }
    return out;
  }

  skip(n: number) {
    this.length -= n;
    while (n > 0) {
      const left = this.chunks[this.first].length - this.head;
      if (n < left) {
        this.head += n;
        return;
      }
      n -= left;
      this.first++;
      this.head = 0;
    }
    // Read chunks are dropped together: removing them one at a time
    // moves every chunk behind them each time.
    if (this.first === this.chunks.length) {
      this.chunks = [];
      this.first = 0;
    } else if (this.first >= 64) {
      this.chunks = this.chunks.slice(this.first);
      this.first = 0;
    }
  }

  take(n: number): Uint8Array {
    const out = this.peek(n);
    this.skip(n);
    return out;
  }

  /** Where `byte` first appears at or after `from`, or -1. */
  indexOf(byte: number, from: number): number {
    let base = -this.head;
    for (let i = this.first; i < this.chunks.length; i++) {
      const chunk = this.chunks[i];
      const end = base + chunk.length;
      if (from < end) {
        const at = chunk.indexOf(byte, Math.max(from - base, 0));
        if (at >= 0) return base + at;
      }
      base = end;
    }
    return -1;
  }
}

/** Tier 1: one JSON message per line, optionally followed by raw bytes. */
export class JsonlCodec {
  private buf = new ByteQueue();
  private pending: { message: Incoming; len: number } | null = null;
  /** Bytes at the front already searched for a newline. */
  private searched = 0;

  encode(message: Outgoing, body?: Uint8Array): Uint8Array {
    const [head, raw] = this.encodeParts(message, body);
    return raw ? concat(head, raw) : head;
  }

  /** The encoded message and, apart from it, the body to send after it. */
  encodeParts(message: Outgoing, body?: Uint8Array): [Uint8Array, Uint8Array?] {
    const has = body !== undefined && body.length > 0;
    const msg = has ? { ...message, body_len: body.length } : message;
    const line = encoder.encode(JSON.stringify(msg) + "\n");
    return has ? [line, body] : [line];
  }

  /** Feeds bytes in and returns every complete frame. Invalid lines are skipped. */
  push(chunk: Uint8Array): Frame[] {
    this.buf.push(chunk);
    const frames: Frame[] = [];
    for (;;) {
      if (this.pending) {
        if (this.buf.length < this.pending.len) break;
        const body = this.buf.take(this.pending.len);
        frames.push({ message: this.pending.message, body });
        this.pending = null;
        continue;
      }
      const nl = this.buf.indexOf(10, this.searched);
      if (nl < 0) {
        this.searched = this.buf.length;
        break;
      }
      this.searched = 0;
      const line = decoder.decode(this.buf.take(nl)).trim();
      this.buf.skip(1);
      if (!line) continue;
      let message: Incoming;
      try {
        message = JSON.parse(line);
      } catch {
        continue;
      }
      const len = (message as RequestMessage).body_len ?? 0;
      if (len > 0) this.pending = { message, len };
      else frames.push({ message });
    }
    return frames;
  }
}

function u32(bytes: Uint8Array): number {
  return ((bytes[0] << 24) | (bytes[1] << 16) | (bytes[2] << 8) | bytes[3]) >>> 0;
}

/** Tier 2: [frame_len u32][json_len u32][json][raw body], big-endian. */
export class FramedCodec {
  private buf = new ByteQueue();

  encode(message: Outgoing, body?: Uint8Array): Uint8Array {
    const [head, raw] = this.encodeParts(message, body);
    return raw ? concat(head, raw) : head;
  }

  /** The encoded message and, apart from it, the body to send after it. */
  encodeParts(message: Outgoing, body?: Uint8Array): [Uint8Array, Uint8Array?] {
    const has = body !== undefined && body.length > 0;
    const msg = has ? { ...message, body_len: body.length } : message;
    const json = encoder.encode(JSON.stringify(msg));
    const head = new Uint8Array(8 + json.length);
    const view = new DataView(head.buffer);
    view.setUint32(0, 4 + json.length + (has ? body.length : 0));
    view.setUint32(4, json.length);
    head.set(json, 8);
    return has ? [head, body] : [head];
  }

  push(chunk: Uint8Array): Frame[] {
    this.buf.push(chunk);
    const frames: Frame[] = [];
    while (this.buf.length >= 4) {
      const frameLen = u32(this.buf.peek(4));
      if (this.buf.length < 4 + frameLen) break;
      this.buf.skip(4);
      const jsonLen = frameLen >= 4 ? u32(this.buf.peek(4)) : frameLen;
      if (jsonLen > frameLen - 4) {
        this.buf.skip(frameLen); // not a frame: its lengths disagree
        continue;
      }
      this.buf.skip(4);
      const json = decoder.decode(this.buf.take(jsonLen));
      const body = this.buf.take(frameLen - 4 - jsonLen);
      try {
        frames.push({ message: JSON.parse(json), body: body.length ? body : undefined });
      } catch {
        // skip invalid frames
      }
    }
    return frames;
  }
}

export type Codec = JsonlCodec | FramedCodec;
