import { type Before, next } from "../../../sdk/js/src/index.ts";

export const tenant: Before = (req) => {
  const t = req.headers["x-tenant"];
  if (!t) return { status: 400, json: { error: "x-tenant header required" } };
  return next({ tenant: t });
};
