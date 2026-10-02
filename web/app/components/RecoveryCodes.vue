<script setup lang="ts">
// Shows freshly generated recovery codes once; the person must confirm they saved them.
const props = defineProps<{ codes: string[] }>();
const emit = defineEmits<{ done: [] }>();
const saved = ref(false);

function download() {
	const text = `xlrx drive – Wiederherstellungscodes\nJeder Code gilt einmal.\n\n${props.codes.join('\n')}\n`;
	const url = URL.createObjectURL(new Blob([text], { type: 'text/plain' }));
	const a = document.createElement('a');
	a.href = url;
	a.download = 'xlrx-wiederherstellungscodes.txt';
	a.click();
	URL.revokeObjectURL(url);
}

const printPage = () => window.print();
</script>

<template>
	<h2>Wiederherstellungscodes</h2>
	<p>
		Damit kommst du ins Konto, falls Handy oder Passkey verloren gehen. Jeder Code gilt einmal. Bitte
		sicher aufbewahren (Passwortmanager oder ausgedruckt) – sie werden nur jetzt angezeigt.
	</p>
	<div class="codes code">
		<span v-for="c in codes" :key="c">{{ c }}</span>
	</div>
	<div class="row" style="margin-top: 0.75rem">
		<button @click="download">Als Datei speichern</button>
		<button @click="printPage">Drucken</button>
	</div>
	<label class="row" style="color: var(--text)">
		<input v-model="saved" type="checkbox" /> Ich habe die Codes sicher gespeichert.
	</label>
	<button class="primary full" :disabled="!saved" @click="emit('done')">Fertig</button>
</template>
