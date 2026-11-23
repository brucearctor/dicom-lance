import { defineConfig } from '@playwright/test';

const executablePath = process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH;

export default defineConfig({
  testDir: './e2e',
  testMatch: 'viewer.spec.ts',
  timeout: 45_000,
  retries: 0,
  use: {
    baseURL: 'http://127.0.0.1:5273',
    headless: true,
    launchOptions: {
      ...(executablePath ? { executablePath } : {}),
      args: ['--enable-webgl', '--ignore-gpu-blocklist', '--use-gl=angle', '--use-angle=swiftshader', '--enable-unsafe-swiftshader'],
    },
  },
  webServer: {
    command:
      './node_modules/.bin/tsc --noEmit && ./node_modules/.bin/vite build && ./node_modules/.bin/vite preview --host 127.0.0.1 --port 5273 --strictPort',
    url: 'http://127.0.0.1:5273',
    reuseExistingServer: false,
    timeout: 120_000,
  },
});
