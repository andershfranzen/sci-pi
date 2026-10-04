import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// The Rust daemon embeds web/dist and serves it at "/", so all asset URLs are relative.
export default defineConfig({
  base: "./",
  plugins: [react()],
  build: {
    outDir: "dist",
    emptyOutDir: true,
  },
  server: {
    // `bun run dev` against the mock (`bun run mock`) or a local daemon.
    proxy: {
      "/api": { target: process.env.SCIPI_API ?? "http://127.0.0.1:7499", ws: true },
      "/hub": { target: process.env.SCIPI_API ?? "http://127.0.0.1:7499" },
    },
  },
});
