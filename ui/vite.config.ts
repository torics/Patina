import { defineConfig } from "vitest/config";

// In development, run `patina serve` and this dev server side by side; API calls are proxied to the daemon.
export default defineConfig({
  server: { proxy: { "/api": { target: "http://127.0.0.1:7878", changeOrigin: false } } },
  build: { outDir: "dist", emptyOutDir: true, sourcemap: false, target: "es2022" },
  test: { include: ["src/**/*.test.ts"] },
});
