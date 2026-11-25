/**
 * End-to-end tests in plain-browser mode: Vite serves the UI, the shell injects
 * a sample project and features fall back to in-memory backends (no Tauri, no
 * network). Run with `pnpm e2e`.
 */
import { defineConfig, devices } from "@playwright/test";

const PORT = Number(process.env.OPENIDEA_E2E_PORT ?? 1450);

export default defineConfig({
  testDir: "./e2e",
  fullyParallel: true,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 1 : 0,
  workers: process.env.CI ? 2 : undefined,
  timeout: 30_000,
  expect: { timeout: 10_000 },
  reporter: process.env.CI ? [["list"], ["html", { open: "never" }]] : "list",
  use: {
    // 127.0.0.1, not `localhost`: Node binds the first address it resolves the
    // host to — on Windows often only `[::1]` — while clients prefer `127.0.0.1`,
    // so half the connections to a `localhost` server are refused (the specs then
    // die at `goto` with ERR_CONNECTION_REFUSED or fail loading `main.tsx`).
    baseURL: `http://127.0.0.1:${PORT}`,
    viewport: { width: 1400, height: 900 },
    // On this Windows machine the retained trace files vanish before context
    // close (the sandboxed runner loses .playwright-artifacts-* trace/network
    // files), failing whole specs with a close-time ENOENT and dragging page
    // loads down under load. Linux CI keeps traces for debugging.
    trace: process.platform === "win32" ? "off" : "retain-on-failure",
    // The workbench keeps indeterminate progress animations running; screenshots
    // are taken explicitly with `animations: "disabled"` instead.
    screenshot: "off",
  },
  projects: [
    {
      name: "chromium",
      use: {
        ...devices["Desktop Chrome"],
        viewport: { width: 1400, height: 900 },
        // The specs use the macOS keymap (⌘-shortcuts) on every host OS.
        userAgent:
          "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36",
      },
    },
  ],
  webServer: {
    command: `pnpm exec vite --host 127.0.0.1 --port ${PORT} --strictPort`,
    url: `http://127.0.0.1:${PORT}`,
    reuseExistingServer: !process.env.CI,
    timeout: 60_000,
    stdout: "ignore",
    stderr: "pipe",
  },
});
