<script lang="ts">
	import { goto } from '$app/navigation';
	import { message, post, type Me } from '#lib/api.ts';
	import { authenticate, passkeysSupported } from '#lib/passkey.ts';
	import { session } from '#lib/session.svelte.ts';

	type Step = 'password' | 'second' | 'recovery';
	let step = $state<Step>('password');
	let username = $state('');
	let password = $state('');
	let code = $state('');
	let challenge = $state('');
	let methods = $state({ totp: false, passkey: false });
	let error = $state('');
	let busy = $state(false);

	$effect(() => {
		if (session.me) goto('/');
	});

	async function attempt(fn: () => Promise<void>) {
		busy = true;
		error = '';
		try {
			await fn();
		} catch (e) {
			error = message(e);
		} finally {
			busy = false;
		}
	}

	function done(me: Me) {
		session.me = me;
		session.loaded = true;
		goto('/');
	}

	const submitPassword = (e: SubmitEvent) => {
		e.preventDefault();
		attempt(async () => {
			const r = await post<{ challenge: string; totp: boolean; passkey: boolean }>('/auth/login', {
				username,
				password
			});
			password = '';
			challenge = r.challenge;
			methods = { totp: r.totp, passkey: r.passkey };
			step = 'second';
		});
	};

	const submitCode = (e: SubmitEvent) => {
		e.preventDefault();
		attempt(async () => {
			const path = step === 'recovery' ? '/auth/recovery' : '/auth/totp';
			done(await post<Me>(path, { challenge, code }));
		});
	};

	async function passkeyFlow(body: object) {
		const begin = await post<{ ceremony: string; options: { publicKey: Record<string, unknown> } }>(
			'/auth/passkey/begin',
			body
		);
		const credential = await authenticate(begin.options);
		done(await post<Me>('/auth/passkey/finish', { ceremony: begin.ceremony, credential }));
	}

	const passkeyOnly = () =>
		attempt(async () => {
			if (!username) throw new Error('Bitte zuerst den Benutzernamen eingeben.');
			await passkeyFlow({ username });
		});

	const passkeySecond = () => attempt(() => passkeyFlow({ challenge }));
</script>

<main class="narrow">
	<div class="card">
		<h1>Anmelden</h1>
		{#if step === 'password'}
			<form onsubmit={submitPassword}>
				<label for="u">Benutzername</label>
				<input id="u" autocomplete="username webauthn" bind:value={username} required />
				<label for="p">Passwort</label>
				<input id="p" type="password" autocomplete="current-password" bind:value={password} required />
				<button class="primary full" disabled={busy}>Weiter</button>
			</form>
			{#if passkeysSupported()}
				<div class="divider">oder</div>
				<button class="full" onclick={passkeyOnly} disabled={busy}>Mit Passkey anmelden</button>
			{/if}
		{:else}
			<p class="muted">Bitte mit dem zweiten Faktor bestätigen.</p>
			{#if methods.passkey && step === 'second'}
				<button class="primary full" onclick={passkeySecond} disabled={busy}>Mit Passkey bestätigen</button>
			{/if}
			{#if methods.totp || step === 'recovery'}
				{#if methods.passkey && step === 'second'}<div class="divider">oder</div>{/if}
				<form onsubmit={submitCode}>
					<label for="c">
						{step === 'recovery' ? 'Wiederherstellungscode' : 'Code aus der Authenticator-App'}
					</label>
					<input
						id="c"
						class="code"
						autocomplete="one-time-code"
						inputmode={step === 'recovery' ? 'text' : 'numeric'}
						bind:value={code}
						required
					/>
					<button class="primary full" disabled={busy}>Anmelden</button>
				</form>
			{/if}
			<p class="muted">
				{#if step === 'second'}
					<button class="link" onclick={() => { step = 'recovery'; code = ''; }}>Wiederherstellungscode verwenden</button>
				{:else}
					<button class="link" onclick={() => { step = 'second'; code = ''; }}>Zurück</button>
				{/if}
			</p>
		{/if}
		{#if error}<p class="error" role="alert">{error}</p>{/if}
	</div>
</main>

<style>
	.link { border: 0; background: none; padding: 0; color: var(--accent); text-decoration: underline; }
</style>
