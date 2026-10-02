<script lang="ts">
	import { session } from '#lib/session.svelte.ts';
</script>

<main class="page">
	<h1>Hallo {session.me?.display_name}</h1>
	<div class="card stack">
		<p>
			Die Anmeldung steht. Als Nächstes kommen hier die Startseite mit Vorschlägen und
			Aktivitäten, das Durchsuchen der Ablagen und die Suche (Meilensteine M1–M3).
		</p>
		{#if session.me && session.me.recovery_codes_left < 4}
			<p class="error">
				Nur noch {session.me.recovery_codes_left} Wiederherstellungscodes übrig.
				<a href="/settings/security">Neue erzeugen</a>
			</p>
		{/if}
		{#if session.me && !session.me.passkeys.length}
			<p class="muted">
				Tipp: Mit einem <a href="/settings/security">Passkey</a> meldest du dich per Face ID oder Touch ID an –
				ohne Passwort und sicher gegen Phishing.
			</p>
		{/if}
	</div>
</main>
