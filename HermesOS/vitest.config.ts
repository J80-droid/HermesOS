import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";
import path from "path";

export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
      path: path.resolve(__dirname, "./src/empty-module.js"),
      fs: path.resolve(__dirname, "./src/empty-module.js"),
      url: path.resolve(__dirname, "./src/empty-module.js"),
      "source-map-js": path.resolve(__dirname, "./src/empty-module.js"),
    },
  },
  test: {
    globals: true,
    environment: "jsdom",
    setupFiles: ["./tests/frontend/setup.ts"],
    include: [
      "./tests/frontend/**/*.test.{ts,tsx}",
      "./tests/frontend/**/*.spec.{ts,tsx}",
    ],
    coverage: {
      provider: "v8",
      reporter: ["text", "lcov", "html"],
      include: ["src/**/*.{ts,tsx}"],
      exclude: ["src/empty-module.js", "src/**/*.d.ts"],
    },
  },
});
