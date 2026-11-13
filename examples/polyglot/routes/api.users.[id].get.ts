import type { Handler } from "../../../sdk/js/src/index.ts";

export default (async (req, ctx) => ({
  json: { id: req.params.id, name: "Ada", tenant: ctx.tenant },
})) satisfies Handler<{ params: { id: number } }>;
