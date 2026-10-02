<script lang="ts">
	import '../app.css';
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { post } from '#lib/api.ts';
	import { loadMe, session } from '#lib/session.svelte.ts';

	let { children } = $props();
	const publicPaths = ['/login', '/setup'];
	let isPublic = $derived(publicPaths.includes(page.url.pathname));

	$effect(() => {
		if (!session.loaded) loadMe();
	});
	$effect(() => {
		if (session.loaded && !session.me && !isPublic) goto('/login');
	});

	async function logout() {
		await post('/auth/logout');
		session.me = null;
		goto('/login');
	}
</script>

{#if session.me && !isPublic}
	<header>
		<nav class="page row">
			<a class="brand" href="/">xlrx drive</a>
			<span class="spacer"></span>
			<a href="/settings/security">Sicherheit</a>
			{#if session.me.is_admin}<a href="/admin">Verwaltung</a>{/if}
			<span class="muted">{session.me.display_name}</span>
			<button onclick={logout}>Abmelden</button>
		</nav>
	</header>
{/if}

{#if isPublic || session.me}
	{@render children()}
{/if}

<style>
	header { background: var(--surface); border-bottom: 1px solid var(--border); }
	nav { margin-top: 0; margin-bottom: 0; padding-top: 0.6rem; padding-bottom: 0.6rem; gap: 1rem; }
	.brand { font-weight: 700; text-decoration: none; color: var(--text); }
	.spacer { flex: 1; }
	nav a { text-decoration: none; }
</style>
