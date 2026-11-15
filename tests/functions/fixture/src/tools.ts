export function weather(req: any, ctx: any) {
  return { json: { city: req.json.city, sky: "clear", tool: ctx.mcp?.tool } };
}
