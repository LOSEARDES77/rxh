import { svelte } from "@sveltejs/vite-plugin-svelte";
import { defineConfig } from "vitest/config";

export default defineConfig({
  base: "./",
  experimental: {
    // The page is served at `/dashboard` and its files at `/dashboard/app.*`:
    // relative URLs keep that working behind any path prefix. Vite refuses
    // `base: "./dashboard/"`, so the prefix is added here.
    renderBuiltUrl: (filename, { hostType }) =>
      hostType === "html" ? `./dashboard/${filename}` : { relative: true },
  },
  plugins: [svelte()],
  build: {
    outDir: "dist",
    emptyOutDir: true,
    sourcemap: false,
    cssCodeSplit: false,
    modulePreload: false,
    rolldownOptions: {
      output: {
        entryFileNames: "app.js",
        assetFileNames: "app[extname]",
        codeSplitting: false,
      },
    },
  },
  server: {
    // `bun run dev` serves the page on `/` and sends the API calls to a daemon.
    proxy: {
      "^/(?!$|index\\.html|src/|@|node_modules/)": "http://127.0.0.1:2019",
    },
  },
  test: {
    environment: "jsdom",
  },
});
