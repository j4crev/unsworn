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

export type Incoming = RequestMessage | CancelMessage | VerifyMessage | { type: string; id?: string };

export type Outgoing = Record<string, unknown> & { type: string };

/** A message plus the raw body bytes that follow it. */
export interface Frame {
  message: Incoming;
  body?: Uint8Array;
}

const encoder = new TextEncoder();
const decoder = new TextDecoder();

function concat(a: Uint8Array, b: Uint8Array): Uint8Array {
  if (a.length === 0) return b;
  const out = new Uint8Array(a.length + b.length);
  out.set(a, 0);
  out.set(b, a.length);
  return out;
}

/** Tier 1: one JSON message per line, optionally followed by raw bytes. */
export class JsonlCodec {
  private buf = new Uint8Array(0);
  private pending: { message: Incoming; len: number } | null = null;

  encode(message: Outgoing, body?: Uint8Array): Uint8Array {
    const msg = body && body.length > 0 ? { ...message, body_len: body.length } : message;
    const line = encoder.encode(JSON.stringify(msg) + "\n");
    return body && body.length > 0 ? concat(line, body) : line;
  }

  /** Feeds bytes in and returns every complete frame. Invalid lines are skipped. */
  push(chunk: Uint8Array): Frame[] {
    this.buf = concat(this.buf, chunk);
    const frames: Frame[] = [];
    for (;;) {
      if (this.pending) {
        if (this.buf.length < this.pending.len) break;
        const body = this.buf.slice(0, this.pending.len);
        this.buf = this.buf.slice(this.pending.len);
        frames.push({ message: this.pending.message, body });
        this.pending = null;
        continue;
      }
      const nl = this.buf.indexOf(10);
      if (nl < 0) break;
      const line = decoder.decode(this.buf.subarray(0, nl)).trim();
      this.buf = this.buf.slice(nl + 1);
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

/** Tier 2: [frame_len u32][json_len u32][json][raw body], big-endian. */
export class FramedCodec {
  private buf = new Uint8Array(0);

  encode(message: Outgoing, body?: Uint8Array): Uint8Array {
    const msg = body && body.length > 0 ? { ...message, body_len: body.length } : message;
    const json = encoder.encode(JSON.stringify(msg));
    const bodyLen = body?.length ?? 0;
    const out = new Uint8Array(8 + json.length + bodyLen);
    const view = new DataView(out.buffer);
    view.setUint32(0, 4 + json.length + bodyLen);
    view.setUint32(4, json.length);
    out.set(json, 8);
    if (body && bodyLen > 0) out.set(body, 8 + json.length);
    return out;
  }

  push(chunk: Uint8Array): Frame[] {
    this.buf = concat(this.buf, chunk);
    const frames: Frame[] = [];
    while (this.buf.length >= 4) {
      const view = new DataView(this.buf.buffer, this.buf.byteOffset, this.buf.byteLength);
      const frameLen = view.getUint32(0);
      if (this.buf.length < 4 + frameLen) break;
      const jsonLen = view.getUint32(4);
      const json = decoder.decode(this.buf.subarray(8, 8 + jsonLen));
      const body = this.buf.slice(8 + jsonLen, 4 + frameLen);
      this.buf = this.buf.slice(4 + frameLen);
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
