import os from 'node:os';
import path from 'node:path';
import { defineConfig } from '@playwright/test';

const LIVE_DATA_DIR = path.join(os.tmpdir(), 'dicom-lance-e2e-live');
const executablePath = process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH;

export default defineConfig({
  testDir: './e2e',
  testMatch: 'live-lance.spec.ts',
  timeout: 60_000,
  retries: 0,
  use: {
    baseURL: 'http://127.0.0.1:18090',
    headless: true,
    launchOptions: {
      ...(executablePath ? { executablePath } : {}),
      args: ['--enable-webgl', '--ignore-gpu-blocklist', '--use-gl=angle'],
    },
  },
  webServer: {
    command: `node -e "require('node:fs').rmSync(process.argv[1], { recursive: true, force: true })" "${LIVE_DATA_DIR}" && ../target/debug/dicom-lance --port 18090 --data-dir "${LIVE_DATA_DIR}" --static-dir ./dist --seed-on-start`,
    url: 'http://127.0.0.1:18090/healthz',
    reuseExistingServer: false,
    timeout: 60_000,
  },
});
