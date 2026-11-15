// A fetch() app, as Hono or itty-router export.
export default {
  fetch(request: Request) {
    const url = new URL(request.url);
    return new Response(`app ${url.pathname} ${process.env.GREETING}`, { headers: { "x-app": "1" } });
  },
};
