import { defineConfig } from "vite";
import solid from "vite-plugin-solid";

// During `vite dev` the frontend runs on :5173 while the chatt client serves
// the WebSocket and file assets on :8080. Proxy both so the dev loop works
// against a running client with HMR.
export default defineConfig({
  plugins: [solid()],
  server: {
    proxy: {
      "/ws": { target: "ws://127.0.0.1:8080", ws: true },
      "/files": { target: "http://127.0.0.1:8080" },
    },
  },
});
