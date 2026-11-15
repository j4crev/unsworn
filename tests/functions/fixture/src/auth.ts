export function tenant(req: { headers: Record<string, string> }) {
  return { continue: { tenant: req.headers["x-tenant"] ?? "none" } };
}
