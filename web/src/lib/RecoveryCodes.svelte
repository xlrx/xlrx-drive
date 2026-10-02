<script lang="ts">
	let { codes, ondone }: { codes: string[]; ondone: () => void } = $props();
	let saved = $state(false);

	function download() {
		const text = `xlrx drive – Wiederherstellungscodes\nJeder Code gilt einmal.\n\n${codes.join('\n')}\n`;
		const url = URL.createObjectURL(new Blob([text], { type: 'text/plain' }));
		const a = document.createElement('a');
		a.href = url;
		a.download = 'xlrx-wiederherstellungscodes.txt';
		a.click();
		URL.revokeObjectURL(url);
	}
</script>

<h2>Wiederherstellungscodes</h2>
<p>
	Damit kommst du ins Konto, falls Handy oder Passkey verloren gehen. Jeder Code gilt einmal. Bitte sicher
	aufbewahren (Passwortmanager oder ausgedruckt) – sie werden nur jetzt angezeigt.
</p>
<div class="codes code">
	{#each codes as c (c)}<span>{c}</span>{/each}
</div>
<div class="row" style="margin-top: 0.75rem">
	<button onclick={download}>Als Datei speichern</button>
	<button onclick={() => window.print()}>Drucken</button>
</div>
<label class="row" style="color: var(--text)">
	<input type="checkbox" bind:checked={saved} /> Ich habe die Codes sicher gespeichert.
</label>
<button class="primary full" disabled={!saved} onclick={ondone}>Fertig</button>
