<script setup lang="ts">
type Step = 'password' | 'second' | 'recovery';
const { set } = useSession();
const step = ref<Step>('password');
const username = ref('');
const password = ref('');
const code = ref('');
const challenge = ref('');
const methods = ref({ totp: false, passkey: false });
const error = ref('');
const busy = ref(false);
const canPasskey = ref(false);
onMounted(() => (canPasskey.value = passkeysSupported()));

async function attempt(fn: () => Promise<void>) {
	busy.value = true;
	error.value = '';
	try {
		await fn();
	} catch (e) {
		error.value = errorMessage(e);
	} finally {
		busy.value = false;
	}
}

async function done(me: Me) {
	set(me);
	await navigateTo('/');
}

const submitPassword = () =>
	attempt(async () => {
		const r = await apiPost<{ challenge: string; totp: boolean; passkey: boolean }>('/auth/login', {
			username: username.value,
			password: password.value
		});
		password.value = '';
		challenge.value = r.challenge;
		methods.value = { totp: r.totp, passkey: r.passkey };
		step.value = 'second';
	});

const submitCode = () =>
	attempt(async () => {
		const path = step.value === 'recovery' ? '/auth/recovery' : '/auth/totp';
		await done(await apiPost<Me>(path, { challenge: challenge.value, code: code.value }));
	});

async function passkeyFlow(body: object) {
	const begin = await apiPost<PasskeyBegin>('/auth/passkey/begin', body);
	const credential = await authenticatePasskey(begin.options);
	await done(await apiPost<Me>('/auth/passkey/finish', { ceremony: begin.ceremony, credential }));
}

const passkeyOnly = () =>
	attempt(async () => {
		if (!username.value) throw new Error('Bitte zuerst den Benutzernamen eingeben.');
		await passkeyFlow({ username: username.value });
	});

const passkeySecond = () => attempt(() => passkeyFlow({ challenge: challenge.value }));

function switchTo(s: Step) {
	step.value = s;
	code.value = '';
}
</script>

<template>
	<main class="narrow with-scenery">
		<div class="card">
			<h1>Anmelden</h1>
			<template v-if="step === 'password'">
				<form @submit.prevent="submitPassword">
					<label for="u">Benutzername</label>
					<input id="u" v-model="username" autocomplete="username webauthn" required />
					<label for="p">Passwort</label>
					<input id="p" v-model="password" type="password" autocomplete="current-password" required />
					<button class="primary full" :disabled="busy">Weiter</button>
				</form>
				<template v-if="canPasskey">
					<div class="divider">oder</div>
					<button class="full" :disabled="busy" @click="passkeyOnly">Mit Passkey anmelden</button>
				</template>
			</template>
			<template v-else>
				<p class="muted">Bitte mit dem zweiten Faktor bestätigen.</p>
				<button
					v-if="methods.passkey && step === 'second'"
					class="primary full"
					:disabled="busy"
					@click="passkeySecond"
				>
					Mit Passkey bestätigen
				</button>
				<template v-if="methods.totp || step === 'recovery'">
					<div v-if="methods.passkey && step === 'second'" class="divider">oder</div>
					<form @submit.prevent="submitCode">
						<label for="c">
							{{ step === 'recovery' ? 'Wiederherstellungscode' : 'Code aus der Authenticator-App' }}
						</label>
						<input
							id="c"
							v-model="code"
							class="code"
							autocomplete="one-time-code"
							:inputmode="step === 'recovery' ? 'text' : 'numeric'"
							required
						/>
						<button class="primary full" :disabled="busy">Anmelden</button>
					</form>
				</template>
				<p class="muted">
					<button v-if="step === 'second'" class="link" @click="switchTo('recovery')">
						Wiederherstellungscode verwenden
					</button>
					<button v-else class="link" @click="switchTo('second')">Zurück</button>
				</p>
			</template>
			<p v-if="error" class="error" role="alert">{{ error }}</p>
		</div>
		<Landscape class="scenery" />
	</main>
</template>

<style scoped>
.link { border: 0; background: none; padding: 0; color: var(--accent); text-decoration: underline; }
</style>
