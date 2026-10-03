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
	<Modal title="Bitte bestätigen" @cancel="emit('cancel')">
		<p class="muted lead">Für diese Aktion ist eine erneute Bestätigung mit dem zweiten Faktor nötig.</p>
		<button v-if="me?.passkeys.length" class="primary big full" :disabled="busy" @click="withPasskey">
			<Icon name="key" />Mit Passkey bestätigen
		</button>
		<template v-if="me?.totp">
			<div v-if="me?.passkeys.length" class="divider">oder</div>
			<form @submit.prevent="withTotp">
				<label for="stepup-code">Code aus der Authenticator-App</label>
				<input id="stepup-code" v-model="code" class="code" inputmode="numeric" autocomplete="one-time-code" required />
				<button class="big full" :disabled="busy">Bestätigen</button>
			</form>
		</template>
		<p v-if="error" class="error" role="alert">{{ error }}</p>
	</Modal>
</template>

<style scoped>
.lead { margin: 0 0 4px; font-size: 14px; }
</style>
