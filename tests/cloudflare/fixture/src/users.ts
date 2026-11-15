import { env } from "cloudflare:workers";

let calls = 0;

export function get(req: any, ctx: any) {
  return { json: { id: req.params.id, tenant: ctx.tenant, greeting: env.GREETING } };
}

export function create(req: any) {
  return { status: 201, json: req.json, headers: { "set-cookie": ["a=1", "b=2"] } };
}

export async function echo(req: any) {
  const text = await new Response(req.stream).text();
  return { text: text.toUpperCase() };
}

export function events() {
  async function* gen() {
    for (const n of [1, 2, 3]) yield `data: ${n}\n\n`;
  }
  return { headers: { "content-type": "text/event-stream" }, body: gen() };
}

export async function slow(req: any) {
  await new Promise((resolve) => setTimeout(resolve, 5000));
  return { text: req.signal.aborted ? "aborted" : "late" };
}

export function boom() {
  throw new Error("kaboom");
}

export function counter() {
  calls += 1;
  return { text: `call ${calls}` };
}
