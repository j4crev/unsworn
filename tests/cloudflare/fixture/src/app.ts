// A fetch() app, as Hono or itty-router export, called with the Worker's env.
export default {
  fetch(request: Request, env: Record<string, unknown>) {
    const url = new URL(request.url);
    return new Response(`app ${url.pathname} ${env.GREETING}`, { headers: { "x-app": "1" } });
  },
};
