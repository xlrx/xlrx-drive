<script lang="ts">
	// Einrichtung über den Link vom Admin: Passwort, dann Authenticator-App oder Passkey.
	import { goto } from '$app/navigation';
	import { message, post, type Me } from '#lib/api.ts';
	import { passkeysSupported, register } from '#lib/passkey.ts';
	import RecoveryCodes from '#lib/RecoveryCodes.svelte';
	import { session } from '#lib/session.svelte.ts';

	type Step = 'loading' | 'invalid' | 'password' | 'factor' | 'totp' | 'codes';
	let step = $state<Step>('loading');
	let token = $state('');
	let account = $state({ username: '', display_name: '', has_password: false });
	let password = $state('');
	let password2 = $state('');
	let totp = $state({ qr_svg: '', secret: '', otpauth_url: '' });
	let code = $state('');
	let passkeyName = $state('');
	let codes = $state<string[]>([]);
	let me: Me | null = null;
	let error = $state('');
	let busy = $state(false);

	$effect(() => {
		const invite = location.hash.slice(1);
		// Den Link aus der Adresszeile entfernen (Verlauf, Bildschirmfreigabe).
		history.replaceState(null, '', '/setup');
		if (!invite) {
			step = 'invalid';
			return;
		}
		post<{ setup_token: string; username: string; display_name: string; has_password: boolean }>(
			'/setup/start',
			{ invite }
		)
			.then((r) => {
				token = r.setup_token;
				account = r;
				step = 'password';
			})
			.catch((e) => {
				error = message(e);
				step = 'invalid';
			});
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

	const setPassword = (e: SubmitEvent) => {
		e.preventDefault();
		attempt(async () => {
			if (password !== password2) throw new Error('Die Passwörter stimmen nicht überein.');
			await post('/setup/password', { setup_token: token, password });
			password = password2 = '';
			step = 'factor';
		});
	};

	const startTotp = () =>
		attempt(async () => {
			totp = await post('/setup/totp/begin', { setup_token: token });
			step = 'totp';
		});

	const confirmTotp = (e: SubmitEvent) => {
		e.preventDefault();
		attempt(async () => {
			const r = await post<{ recovery_codes: string[]; me: Me }>('/setup/totp/confirm', {
				setup_token: token,
				code
			});
			codes = r.recovery_codes;
			me = r.me;
			step = 'codes';
		});
	};

	const addPasskey = () =>
		attempt(async () => {
			const begin = await post<{ ceremony: string; options: { publicKey: Record<string, unknown> } }>(
				'/setup/passkey/begin',
				{ setup_token: token, name: passkeyName || 'Passkey' }
			);
			const credential = await register(begin.options);
			const r = await post<{ recovery_codes: string[]; me: Me }>('/setup/passkey/finish', {
				setup_token: token,
				ceremony: begin.ceremony,
				credential
			});
			codes = r.recovery_codes;
			me = r.me;
			step = 'codes';
		});

	function finish() {
		session.me = me;
		session.loaded = true;
		goto('/');
	}
</script>

<main class="narrow">
	<div class="card">
		<h1>Konto einrichten</h1>
		{#if step === 'loading'}
			<p class="muted">Einen Moment …</p>
		{:else if step === 'invalid'}
			<p class="error">{error || 'Der Einrichtungslink fehlt oder ist ungültig.'}</p>
			<p class="muted">Bitte bei der Person nachfragen, die das Konto verwaltet.</p>
		{:else if step === 'password'}
			<p>Willkommen, {account.display_name}! Benutzername: <strong>{account.username}</strong></p>
			<form onsubmit={setPassword}>
				<label for="p1">Neues Passwort (mindestens 12 Zeichen)</label>
				<input id="p1" type="password" autocomplete="new-password" minlength="12" bind:value={password} required />
				<label for="p2">Passwort wiederholen</label>
				<input id="p2" type="password" autocomplete="new-password" bind:value={password2} required />
				<p class="muted">Tipp: Ein Satz aus mehreren Wörtern ist leicht zu merken und schwer zu erraten.</p>
				<button class="primary full" disabled={busy}>Weiter</button>
			</form>
		{:else if step === 'factor'}
			<p>
				Jede Anmeldung braucht einen zweiten Faktor. Wähle mindestens einen – später lassen sich weitere
				hinzufügen.
			</p>
			{#if passkeysSupported()}
				<label for="pkname">Name des Geräts</label>
				<input id="pkname" placeholder="z.B. iPhone, MacBook" bind:value={passkeyName} />
				<button class="primary full" onclick={addPasskey} disabled={busy}>
					Passkey einrichten (Face ID, Touch ID)
				</button>
				<div class="divider">oder</div>
			{/if}
			<button class="full" onclick={startTotp} disabled={busy}>Authenticator-App einrichten</button>
		{:else if step === 'totp'}
			<p>Den QR-Code mit der Authenticator-App scannen (z.B. Apple Passwörter, 2FAS, Aegis, 1Password).</p>
			<div class="qr">{@html totp.qr_svg}</div>
			<p class="muted">Oder den Schlüssel eingeben: <span class="code">{totp.secret}</span></p>
			<form onsubmit={confirmTotp}>
				<label for="c">Angezeigter Code</label>
				<input id="c" class="code" inputmode="numeric" autocomplete="one-time-code" bind:value={code} required />
				<button class="primary full" disabled={busy}>Bestätigen</button>
			</form>
		{:else if step === 'codes'}
			<RecoveryCodes {codes} ondone={finish} />
		{/if}
		{#if error && step !== 'invalid'}<p class="error" role="alert">{error}</p>{/if}
	</div>
</main>
