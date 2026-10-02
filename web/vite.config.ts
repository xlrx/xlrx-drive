import adapter from '@sveltejs/adapter-static';
import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vite';

export default defineConfig({
	plugins: [
		sveltekit({
			// Single-Page-App, ausgeliefert vom xlrx-server (XLRX_WEB_DIR).
			adapter: adapter({ fallback: 'index.html', strict: true }),
			// Strenge CSP: nur eigene Skripte, das Start-Skript per Hash erlaubt.
			csp: {
				mode: 'hash',
				directives: {
					'default-src': ['self'],
					'script-src': ['self'],
					'style-src': ['self', 'unsafe-inline'],
					'img-src': ['self', 'data:'],
					'connect-src': ['self'],
					'font-src': ['self'],
					'object-src': ['none'],
					'base-uri': ['none'],
					'form-action': ['self']
				}
			}
		})
	],
	server: {
		// Entwicklung: API an den lokalen xlrx-server weiterreichen.
		proxy: { '/api': 'http://127.0.0.1:8080', '/healthz': 'http://127.0.0.1:8080' }
	}
});
