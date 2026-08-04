import { defineConfig, type Plugin } from "vite";
import solid from "vite-plugin-solid";

// The user stylesheet, injected after Vite's own asset tags so a rule in
// `~/.config/chatt/web.css` wins over the bundled rule at equal specificity.
// The client always answers this path, with an empty stylesheet when the file
// does not exist.
const userStylesheet: Plugin = {
  name: "chatt:user-stylesheet",
  transformIndexHtml: {
    order: "post",
    handler: () => [
      {
        tag: "link",
        attrs: { rel: "stylesheet", href: "/web.css" },
        injectTo: "head",
      },
    ],
  },
};

// During `vite dev` the frontend runs on :5173 while the chatt client serves
// the WebSocket and file assets on :8080. Proxy both so the dev loop works
// against a running client with HMR.
export default defineConfig({
  plugins: [solid(), userStylesheet],
  server: {
    proxy: {
      // Running Vite is the explicit dev/test exception to the backend's
      // browser-origin allowlist, so rewrite the proxied WebSocket to the
      // backend's default allowed origin.
      "/ws": {
        target: "http://127.0.0.1:8080",
        ws: true,
        rewriteWsOrigin: true,
      },
      "/files": { target: "http://127.0.0.1:8080" },
      "/highlight": { target: "http://127.0.0.1:8080" },
      "/web.css": { target: "http://127.0.0.1:8080" },
    },
  },
});
