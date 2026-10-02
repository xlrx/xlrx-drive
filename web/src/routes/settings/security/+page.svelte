<script lang="ts">
	import { del, get, patch, post } from '#lib/api.ts';
	import { Guard } from '#lib/guard.svelte.ts';
	import { passkeysSupported, register } from '#lib/passkey.ts';
	import RecoveryCodes from '#lib/RecoveryCodes.svelte';
	import { loadMe, session } from '#lib/session.svelte.ts';
	import StepUp from '#lib/StepUp.svelte';

	type SessionInfo = { id: number; created_at: string; last_seen_at: string; user_agent: string | null; ip: string | null; current: boolean };

	const g = new Guard();
	let sessions = $state<SessionInfo[]>([]);
	let newPasskey = $state('');
	let totpSetup = $state<null | { ceremony: string; qr_svg: string; secret: string }>(null);
	let totpCode = $state('');
	let codes = $state<string[] | null>(null);
	let newPassword = $state('');
	let notice = $state('');

	const fmt = (s: string | null) => (s ? new Date(s).toLocaleString('de-DE') : '–');
	const refresh = async () => {
		await loadMe();
		sessions = await get<SessionInfo[]>('/me/sessions');
	};
	$effect(() => {
		refresh();
	});

	const addPasskey = () =>
		g.run(async () => {
			const begin = await post<{ ceremony: string; options: { publicKey: Record<string, unknown> } }>(
				'/me/passkeys/begin',
				{ name: newPasskey || 'Passkey' }
			);
			const credential = await register(begin.options);
			await post('/me/passkeys/finish', { ceremony: begin.ceremony, credential });
			newPasskey = '';
			notice = 'Passkey hinzugefügt.';
			await refresh();
		});

	const renamePasskey = (id: number, current: string) =>
		g.run(async () => {
			const name = prompt('Neuer Name', current);
			if (!name) return;
			await patch(`/me/passkeys/${id}`, { name });
			await refresh();
		});

	const removePasskey = (id: number) =>
		g.run(async () => {
			if (!confirm('Diesen Passkey entfernen?')) return;
			await del(`/me/passkeys/${id}`);
			await refresh();
		});

	const beginTotp = () =>
		g.run(async () => {
			totpSetup = await post('/me/totp/begin');
		});

	const confirmTotp = (e: SubmitEvent) => {
		e.preventDefault();
		g.run(async () => {
			await post('/me/totp/confirm', { ceremony: totpSetup?.ceremony, code: totpCode });
			totpSetup = null;
			totpCode = '';
			notice = 'Authenticator-App eingerichtet.';
			await refresh();
		});
	};

	const removeTotp = () =>
		g.run(async () => {
			if (!confirm('Authenticator-App entfernen?')) return;
			await del('/me/totp');
			await refresh();
		});

	const newCodes = () =>
		g.run(async () => {
			if (!confirm('Neue Codes erzeugen? Die alten werden ungültig.')) return;
			codes = (await post<{ recovery_codes: string[] }>('/me/recovery-codes')).recovery_codes;
		});

	const changePassword = (e: SubmitEvent) => {
		e.preventDefault();
		g.run(async () => {
			await post('/me/password', { password: newPassword });
			newPassword = '';
			notice = 'Passwort geändert. Andere Sitzungen wurden beendet.';
			await refresh();
		});
	};

	const revoke = (id: number) =>
		g.run(async () => {
			await del(`/me/sessions/${id}`);
			await refresh();
		});
</script>

<main class="page">
	<h1>Sicherheit</h1>
	{#if notice}<p class="ok">{notice}</p>{/if}
	{#if g.error}<p class="error" role="alert">{g.error}</p>{/if}

	{#if codes}
		<div class="card"><RecoveryCodes {codes} ondone={() => { codes = null; refresh(); }} /></div>
	{:else if session.me}
		<div class="card">
			<h2 style="margin-top: 0">Passkeys</h2>
			{#if session.me.passkeys.length}
				<table>
					<thead><tr><th>Name</th><th>Hinzugefügt</th><th>Zuletzt benutzt</th><th></th></tr></thead>
					<tbody>
						{#each session.me.passkeys as pk (pk.id)}
							<tr>
								<td>{pk.name}</td>
								<td>{fmt(pk.created_at)}</td>
								<td>{fmt(pk.last_used_at)}</td>
								<td class="row">
									<button onclick={() => renamePasskey(pk.id, pk.name)}>Umbenennen</button>
									<button class="danger" onclick={() => removePasskey(pk.id)}>Entfernen</button>
								</td>
							</tr>
						{/each}
					</tbody>
				</table>
			{:else}
				<p class="muted">Noch kein Passkey. Mit Passkeys meldest du dich per Face ID oder Touch ID an.</p>
			{/if}
			{#if passkeysSupported()}
				<div class="row" style="margin-top: 1rem">
					<input placeholder="Name, z.B. MacBook" bind:value={newPasskey} style="max-width: 16rem" />
					<button class="primary" onclick={addPasskey} disabled={g.busy}>Passkey hinzufügen</button>
				</div>
			{/if}

			<h2>Authenticator-App</h2>
			{#if totpSetup}
				<p>Den QR-Code scannen und den angezeigten Code eingeben.</p>
				<div class="qr">{@html totpSetup.qr_svg}</div>
				<p class="muted">Schlüssel: <span class="code">{totpSetup.secret}</span></p>
				<form class="row" onsubmit={confirmTotp}>
					<input class="code" inputmode="numeric" autocomplete="one-time-code" bind:value={totpCode} required style="max-width: 10rem" />
					<button class="primary" disabled={g.busy}>Bestätigen</button>
					<button type="button" onclick={() => (totpSetup = null)}>Abbrechen</button>
				</form>
			{:else if session.me.totp}
				<p class="row">Eingerichtet. <button onclick={beginTotp}>Neu einrichten</button> <button class="danger" onclick={removeTotp}>Entfernen</button></p>
			{:else}
				<p class="row">Nicht eingerichtet. <button onclick={beginTotp}>Einrichten</button></p>
			{/if}

			<h2>Wiederherstellungscodes</h2>
			<p class="row">
				Noch {session.me.recovery_codes_left} von 10 übrig.
				<button onclick={newCodes}>Neue erzeugen</button>
			</p>

			<h2>Passwort</h2>
			<form class="row" onsubmit={changePassword}>
				<input type="password" autocomplete="new-password" minlength="12" placeholder="Neues Passwort" bind:value={newPassword} required style="max-width: 20rem" />
				<button disabled={g.busy}>Ändern</button>
			</form>
		</div>

		<div class="card" style="margin-top: 1.5rem">
			<h2 style="margin-top: 0">Angemeldete Sitzungen</h2>
			<table>
				<thead><tr><th>Gerät</th><th>IP</th><th>Zuletzt aktiv</th><th></th></tr></thead>
				<tbody>
					{#each sessions as s (s.id)}
						<tr>
							<td>{s.user_agent ?? 'unbekannt'} {#if s.current}<span class="badge">diese</span>{/if}</td>
							<td>{s.ip ?? '–'}</td>
							<td>{fmt(s.last_seen_at)}</td>
							<td>{#if !s.current}<button class="danger" onclick={() => revoke(s.id)}>Abmelden</button>{/if}</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	{/if}
</main>

{#if g.pending}<StepUp ondone={g.confirmed} oncancel={g.cancel} />{/if}
