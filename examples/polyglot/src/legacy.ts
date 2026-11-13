// Stands in for an existing app with its own router (Hono, Elysia, ...):
// anything with a fetch(Request) method can sit behind a catch-all.
export default {
  fetch(req: Request): Response {
    const url = new URL(req.url);
    return Response.json({ from: "legacy app", path: url.pathname });
  },
};
