<script lang="ts">
	import { get, post } from '#lib/api.ts';
	import { Guard } from '#lib/guard.svelte.ts';
	import { session } from '#lib/session.svelte.ts';
	import StepUp from '#lib/StepUp.svelte';

	type User = {
		id: number; username: string; display_name: string; email: string | null; is_admin: boolean;
		disabled: boolean; totp: boolean; passkeys: number; setup_pending: boolean; created_at: string;
	};
	type Audit = { id: number; at: string; actor: string | null; target: string | null; action: string; ip: string | null };

	const g = new Guard();
	let users = $state<User[]>([]);
	let audit = $state<Audit[]>([]);
	let form = $state({ username: '', display_name: '', email: '', is_admin: false });
	let link = $state<null | { who: string; url: string }>(null);

	const refresh = async () => {
		users = await get<User[]>('/admin/users');
		audit = await get<Audit[]>('/admin/audit?limit=50');
	};
	$effect(() => {
		if (session.me?.is_admin) g.run(refresh);
	});

	const create = (e: SubmitEvent) => {
		e.preventDefault();
		g.run(async () => {
			const r = await post<{ setup_url: string }>('/admin/users', { ...form, email: form.email || null });
			link = { who: form.username, url: r.setup_url };
			form = { username: '', display_name: '', email: '', is_admin: false };
			await refresh();
		});
	};

	const newLink = (u: User, reset: boolean) =>
		g.run(async () => {
			if (reset && !confirm(`Alle zweiten Faktoren von ${u.username} zurücksetzen? Alle Sitzungen werden beendet.`)) return;
			const r = await post<{ setup_url: string }>(`/admin/users/${u.id}/${reset ? 'reset-factors' : 'invite'}`);
			link = { who: u.username, url: r.setup_url };
			await refresh();
		});

	const toggle = (u: User) =>
		g.run(async () => {
			await post(`/admin/users/${u.id}/disabled`, { disabled: !u.disabled });
			await refresh();
		});

	const copy = () => link && navigator.clipboard.writeText(link.url);
	const fmt = (s: string) => new Date(s).toLocaleString('de-DE');
</script>

<main class="page">
	<h1>Verwaltung</h1>
	{#if !session.me?.is_admin}
		<p class="error">Nur für Administratoren.</p>
	{:else}
		{#if g.error}<p class="error" role="alert">{g.error}</p>{/if}
		{#if link}
			<div class="card stack">
				<strong>Einrichtungslink für {link.who}</strong>
				<p class="muted">72 Stunden gültig und nur einmal verwendbar. Bitte auf sicherem Weg weitergeben.</p>
				<input readonly value={link.url} class="code" />
				<div class="row"><button onclick={copy}>Kopieren</button><button onclick={() => (link = null)}>Schließen</button></div>
			</div>
		{/if}

		<div class="card" style="margin-top: 1.5rem">
			<h2 style="margin-top: 0">Konten</h2>
			<table>
				<thead><tr><th>Konto</th><th>Faktoren</th><th>Status</th><th></th></tr></thead>
				<tbody>
					{#each users as u (u.id)}
						<tr>
							<td><strong>{u.display_name}</strong><br /><span class="muted">{u.username}</span>
								{#if u.is_admin}<span class="badge">Admin</span>{/if}</td>
							<td>{[u.totp ? 'App' : null, u.passkeys ? `${u.passkeys} Passkey(s)` : null].filter(Boolean).join(', ') || '–'}</td>
							<td>{u.disabled ? 'gesperrt' : u.setup_pending ? 'Einrichtung offen' : 'aktiv'}</td>
							<td class="row">
								<button onclick={() => newLink(u, false)}>Neuer Link</button>
								<button onclick={() => newLink(u, true)}>Faktoren zurücksetzen</button>
								{#if u.id !== session.me?.id}
									<button class="danger" onclick={() => toggle(u)}>{u.disabled ? 'Entsperren' : 'Sperren'}</button>
								{/if}
							</td>
						</tr>
					{/each}
				</tbody>
			</table>

			<h2>Neues Konto</h2>
			<form onsubmit={create}>
				<div class="grid">
					<div><label for="nu">Benutzername</label><input id="nu" bind:value={form.username} required /></div>
					<div><label for="nd">Anzeigename</label><input id="nd" bind:value={form.display_name} required /></div>
					<div><label for="ne">E-Mail (optional)</label><input id="ne" type="email" bind:value={form.email} /></div>
				</div>
				<label class="row" style="color: var(--text)"><input type="checkbox" bind:checked={form.is_admin} /> Administrator</label>
				<button class="primary" disabled={g.busy}>Anlegen und Einrichtungslink erzeugen</button>
			</form>
		</div>

		<div class="card" style="margin-top: 1.5rem">
			<h2 style="margin-top: 0">Protokoll</h2>
			<table>
				<thead><tr><th>Zeit</th><th>Aktion</th><th>Wer</th><th>Betrifft</th><th>IP</th></tr></thead>
				<tbody>
					{#each audit as a (a.id)}
						<tr><td>{fmt(a.at)}</td><td>{a.action}</td><td>{a.actor ?? '–'}</td><td>{a.target ?? '–'}</td><td>{a.ip ?? '–'}</td></tr>
					{/each}
				</tbody>
			</table>
		</div>
	{/if}
</main>

{#if g.pending}<StepUp ondone={g.confirmed} oncancel={g.cancel} />{/if}

<style>
	.grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(12rem, 1fr)); gap: 0 1rem; }
</style>
