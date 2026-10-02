<script setup lang="ts">
const { me } = useSession();
const clock = useClock();
const today = computed(() => clock.value.toLocaleDateString('de-DE', { weekday: 'long', day: 'numeric', month: 'long' }));
</script>

<template>
	<main class="page with-scenery">
		<p class="muted date">{{ today }}</p>
		<h1 class="greeting">{{ greeting(timeOfDay(clock), me?.display_name ?? '') }}</h1>
		<div class="card stack">
			<p>
				<NuxtLink to="/files">Meine Ablage öffnen</NuxtLink> – durchsuchen, hochladen, ansehen und ändern.
			</p>
			<p class="muted">
				Als Nächstes kommen der Sync für Mac und iPhone, dann hier Vorschläge, Aktivitäten und die Suche.
			</p>
			<p v-if="me && me.recovery_codes_left < 4" class="error">
				Nur noch {{ me.recovery_codes_left }} Wiederherstellungscodes übrig.
				<NuxtLink to="/settings/security">Neue erzeugen</NuxtLink>
			</p>
			<p v-if="me && !me.passkeys.length" class="muted">
				Tipp: Mit einem <NuxtLink to="/settings/security">Passkey</NuxtLink> meldest du dich per Face ID
				oder Touch ID an – ohne Passwort und sicher gegen Phishing.
			</p>
		</div>
		<Landscape class="scenery" />
	</main>
</template>

<style scoped>
.date { margin: 0; }
.greeting { font-size: 2rem; font-weight: 400; letter-spacing: -0.02em; margin: 0.2rem 0 1.25rem; }
</style>
