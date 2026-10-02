<script lang="ts">
	// Bestätigung mit zweitem Faktor für sensible Aktionen.
	import { message, post } from './api';
	import { authenticate } from './passkey';
	import { session } from './session.svelte';

	let { ondone, oncancel }: { ondone: () => void; oncancel: () => void } = $props();
	let code = $state('');
	let error = $state('');
	let busy = $state(false);

	async function withTotp(e: SubmitEvent) {
		e.preventDefault();
		busy = true;
		error = '';
		try {
			await post('/auth/step-up/totp', { code });
			ondone();
		} catch (err) {
			error = message(err);
		} finally {
			busy = false;
		}
	}

	async function withPasskey() {
		busy = true;
		error = '';
		try {
			const begin = await post<{ ceremony: string; options: { publicKey: Record<string, unknown> } }>(
				'/auth/step-up/passkey/begin'
			);
			const credential = await authenticate(begin.options);
			await post('/auth/step-up/passkey/finish', { ceremony: begin.ceremony, credential });
			ondone();
		} catch (err) {
			error = message(err);
		} finally {
			busy = false;
		}
	}
</script>

<div class="backdrop" role="presentation">
	<div class="card dialog" role="dialog" aria-modal="true" aria-labelledby="stepup-title">
		<h2 id="stepup-title">Bitte bestätigen</h2>
		<p class="muted">Für diese Aktion ist eine erneute Bestätigung mit dem zweiten Faktor nötig.</p>
		{#if session.me?.passkeys.length}
			<button class="primary full" onclick={withPasskey} disabled={busy}>Mit Passkey bestätigen</button>
		{/if}
		{#if session.me?.totp}
			{#if session.me?.passkeys.length}<div class="divider">oder</div>{/if}
			<form onsubmit={withTotp}>
				<label for="stepup-code">Code aus der Authenticator-App</label>
				<input id="stepup-code" class="code" inputmode="numeric" autocomplete="one-time-code" bind:value={code} required />
				<button class="full" disabled={busy}>Bestätigen</button>
			</form>
		{/if}
		{#if error}<p class="error">{error}</p>{/if}
		<button class="full" onclick={oncancel}>Abbrechen</button>
	</div>
</div>

<style>
	.backdrop {
		position: fixed;
		inset: 0;
		background: rgb(0 0 0 / 40%);
		display: grid;
		place-items: center;
		padding: 1rem;
		z-index: 10;
	}
	.dialog { width: min(24rem, 100%); }
</style>
