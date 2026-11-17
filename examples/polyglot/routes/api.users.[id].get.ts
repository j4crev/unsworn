import type { ApiUsersIdGet } from "../src/unsworn.gen.ts";

// `req.params.id` is a number because the config says so: the type is
// written from it.
export default (async (req, ctx) => ({
  json: { id: req.params.id, name: "Ada", tenant: ctx.tenant },
})) satisfies ApiUsersIdGet;
