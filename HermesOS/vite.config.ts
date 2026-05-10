import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import path from "path";

export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
      "path": path.resolve(__dirname, "./src/empty-module.js"),
      "fs": path.resolve(__dirname, "./src/empty-module.js"),
      "url": path.resolve(__dirname, "./src/empty-module.js"),
      "source-map-js": path.resolve(__dirname, "./src/empty-module.js"),
    },
    dedupe: [
      "react",
      "react-dom",
      "@react-three/fiber",
      "@observablehq/plot",
      "three",
      "leva",
      "gsap",
    ],
  },
  build: {
    // Must stay under HermesOS/ — matches `frontendDist: "../dist"` in src-tauri/tauri.conf.json.
    outDir: "dist",
    emptyOutDir: true,
  },
  clearScreen: false,
  server: {
    port: 5185,
    strictPort: true,
    host: "127.0.0.1",
    hmr: {
      host: "127.0.0.1",
      port: 5185,
    },
  },
  preview: {
    port: 5185,
  },
});