// Functions, an outbound service and a shadow, in a Worker.

export const count = (req: any) => ({ json: { words: String(req.json.text).split(" ").length } });

export const whoami = (req: any, ctx: any) => ({ json: { ctx, route: req.route, headers: req.headers } });

export async function route(req: any, ctx: any) {
  const what = req.params.what;
  try {
    if (what === "count") return { json: await ctx.call("count", { text: req.query.q }) };
    if (what === "bad") return { json: await ctx.call("count", { text: 5 }) };
    if (what === "whoami") return { json: await ctx.call("whoami") };
    if (what === "charge") {
      const r = await ctx.fetch("billing", "/charges?limit=2", { headers: { authorization: "Bearer mine" } });
      return { json: { status: r.status, sent: r.json() } };
    }
    if (what === "admin") return { json: await ctx.fetch("billing", "/admin") };
  } catch (err: any) {
    return { json: { failed: err.status, name: err.name, message: err.message } };
  }
  return { status: 404 };
}

export const search = (req: any) => ({ json: { hits: [req.query.q], total: 1 } });

export const searchV2 = (req: any) => ({ json: { total: 1.0, hits: [req.query.q === "bug" ? "WRONG" : req.query.q] } });
