<script setup lang="ts">
const { me } = useSession();
</script>

<template>
	<main class="page">
		<h1>Hallo {{ me?.display_name }}</h1>
		<div class="card stack">
			<p>
				Die Anmeldung steht. Als Nächstes kommen hier die Startseite mit Vorschlägen und Aktivitäten, das
				Durchsuchen der Ablagen und die Suche (Meilensteine M1–M3).
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
	</main>
</template>
