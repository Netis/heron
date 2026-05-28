import path from "path"
import { defineConfig } from "vitest/config"
import react from "@vitejs/plugin-react"

export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
    },
  },
  define: {
    __APP_VERSION__: JSON.stringify("0.0.0-test"),
  },
  test: {
    globals: true,
    environment: "jsdom",
    setupFiles: ["./src/test/setup.ts"],
    css: false,
    // Several pre-existing test files in src/lib/ and src/components/
    // call-renderers/ target bun's built-in test runner (`bun test`) and
    // import from `bun:test`. Vitest can't bundle that built-in. Rather than
    // maintaining a blacklist, we whitelist the directories that hold
    // vitest-style tests.
    include: [
      "src/test/**/*.{test,spec}.{ts,tsx}",
      "src/stores/**/*.{test,spec}.{ts,tsx}",
      "src/hooks/**/*.{test,spec}.{ts,tsx}",
      "src/components/charts/**/*.{test,spec}.{ts,tsx}",
    ],
    exclude: ["**/node_modules/**", "**/dist/**"],
  },
})
