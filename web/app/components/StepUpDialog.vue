<script setup lang="ts">
// Confirmation with a second factor for sensitive actions.
const emit = defineEmits<{ done: []; cancel: [] }>();
const { me } = useSession();
const code = ref('');
const error = ref('');
const busy = ref(false);

async function attempt(fn: () => Promise<void>) {
	busy.value = true;
	error.value = '';
	try {
		await fn();
		emit('done');
	} catch (e) {
		error.value = errorMessage(e);
	} finally {
		busy.value = false;
	}
}

const withTotp = () => attempt(() => apiPost('/auth/step-up/totp', { code: code.value }));

const withPasskey = () =>
	attempt(async () => {
		const begin = await apiPost<PasskeyBegin>('/auth/step-up/passkey/begin');
		const credential = await authenticatePasskey(begin.options);
		await apiPost('/auth/step-up/passkey/finish', { ceremony: begin.ceremony, credential });
	});
</script>

<template>
	<div class="backdrop" role="presentation">
		<div class="card dialog" role="dialog" aria-modal="true" aria-labelledby="stepup-title">
			<h2 id="stepup-title">Bitte bestätigen</h2>
			<p class="muted">Für diese Aktion ist eine erneute Bestätigung mit dem zweiten Faktor nötig.</p>
			<button v-if="me?.passkeys.length" class="primary full" :disabled="busy" @click="withPasskey">
				Mit Passkey bestätigen
			</button>
			<template v-if="me?.totp">
				<div v-if="me?.passkeys.length" class="divider">oder</div>
				<form @submit.prevent="withTotp">
					<label for="stepup-code">Code aus der Authenticator-App</label>
					<input
						id="stepup-code"
						v-model="code"
						class="code"
						inputmode="numeric"
						autocomplete="one-time-code"
						required
					/>
					<button class="full" :disabled="busy">Bestätigen</button>
				</form>
			</template>
			<p v-if="error" class="error">{{ error }}</p>
			<button class="full" @click="emit('cancel')">Abbrechen</button>
		</div>
	</div>
</template>

<style scoped>
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
