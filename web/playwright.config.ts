import { defineConfig } from '@playwright/test';

// End-to-end against a running xlrx-server (see e2e/run.sh).
export default defineConfig({
	testDir: 'e2e',
	timeout: 60_000,
	use: {
		baseURL: process.env.XLRX_BASE_URL ?? 'http://localhost:8080',
		launchOptions: process.env.PW_CHROMIUM ? { executablePath: process.env.PW_CHROMIUM } : {}
	},
	projects: [{ name: 'chromium', use: { browserName: 'chromium' } }]
});
