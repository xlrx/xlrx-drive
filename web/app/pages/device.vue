<script setup lang="ts">
// Allowing a device: the Mac or iPhone app opens this page (in a browser session) with its PKCE
// challenge; after confirming, the browser goes back to the app with a one-time code.
const route = useRoute();
const g = useGuard();
const step = ref<'ask' | 'done' | 'denied'>('ask');
const redirect = ref('');

const q = (key: string) => {
	const v = route.query[key];
	return typeof v === 'string' ? v : '';
};
const request = computed(() => ({
	challenge: q('challenge'),
	redirect_uri: q('redirect_uri'),
	state: q('state'),
	name: q('name'),
	platform: q('platform')
}));
const complete = computed(() => !!(request.value.challenge && request.value.redirect_uri && request.value.name && request.value.platform));
const PLATFORMS: Record<string, string> = { macos: 'Mac', ios: 'iPhone oder iPad' };
const platform = computed(() => PLATFORMS[request.value.platform] ?? request.value.platform);

const allow = () =>
	g.run(async () => {
		const r = await apiPost<{ redirect: string }>('/devices/authorize', request.value);
		redirect.value = r.redirect;
		step.value = 'done';
		window.location.href = r.redirect;
	});
</script>

<template>
	<main class="narrow with-scenery">
		<div class="card stack">
			<h1>Gerät verbinden</h1>
			<p v-if="!complete" class="error">
				Dieser Link ist unvollständig. Bitte die Anmeldung in der App neu starten.
			</p>
			<template v-else-if="step === 'ask'">
				<p>
					<strong>{{ request.name }}</strong> ({{ platform }}) möchte auf deine Dateien zugreifen.
				</p>
				<p class="muted">
					Bestätige nur, wenn du die Anmeldung gerade selbst in der xlrx-App gestartet hast. Das Gerät
					bleibt angemeldet, bis du es unter „Sicherheit“ abmeldest.
				</p>
				<button class="primary full" :disabled="g.busy.value" @click="allow">Verbinden</button>
				<button class="full" :disabled="g.busy.value" @click="step = 'denied'">Abbrechen</button>
			</template>
			<template v-else-if="step === 'done'">
				<p class="ok">Verbunden. Die App übernimmt jetzt.</p>
				<p class="muted">Falls sie sich nicht öffnet: <a :href="redirect">Zurück zur App</a></p>
			</template>
			<p v-else>Abgebrochen. Du kannst dieses Fenster schließen.</p>
			<p v-if="g.error.value" class="error" role="alert">{{ g.error.value }}</p>
		</div>
		<Landscape class="scenery" />
	</main>
	<StepUpDialog v-if="g.pending.value" @done="g.confirmed" @cancel="g.cancel" />
</template>
