import type { ApiUsersIdGet } from "../src/unsworn.gen.ts";

// `req.params.id` is a number because the config says so: the type is
// written from it. `initials` is a Python handler (src/names.py), called
// through Unsworn.
export default (async (req, ctx) => {
  const name = "Ada Lovelace";
  return {
    json: { id: req.params.id, name, initials: await ctx.call("initials", { name }), tenant: ctx.tenant },
  };
}) satisfies ApiUsersIdGet;
