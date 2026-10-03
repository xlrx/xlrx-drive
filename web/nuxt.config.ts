// Single-page app, generated as static files and served by xlrx-server (XLRX_WEB_DIR).
export default defineNuxtConfig({
	compatibilityDate: '2026-10-01',
	ssr: false,
	devtools: { enabled: false },
	telemetry: false,
	// Geist is bundled with the app (no font service: CSP and privacy).
	css: ['@fontsource-variable/geist/index.css', '@fontsource-variable/geist-mono/index.css', '~/assets/css/main.css'],
	app: {
		head: {
			htmlAttrs: { lang: 'de' },
			title: 'xlrx drive',
			meta: [{ name: 'color-scheme', content: 'light dark' }],
			link: [{ rel: 'icon', href: '/favicon.svg', type: 'image/svg+xml' }]
		}
	},
	typescript: { strict: true },
	nitro: {
		// One index.html for all routes; the server falls back to it for client-side routes.
		prerender: { crawlLinks: false, routes: ['/'] },
		// Development only: forward API calls to a local xlrx-server.
		devProxy: { '/api': { target: 'http://127.0.0.1:8080/api' } }
	}
});
