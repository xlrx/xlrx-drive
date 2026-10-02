<script setup lang="ts">
// A file of that name exists already: replace (the old content stays as a version), keep both,
// or skip. Optionally for all remaining conflicts of this upload.
defineProps<{ name: string; canReplace: boolean; more: boolean }>();
const emit = defineEmits<{ choose: [choice: ConflictChoice, forAll: boolean] }>();
const forAll = ref(false);
</script>

<template>
	<Modal title="Datei gibt es schon" @cancel="emit('choose', 'skip', false)">
		<p>„{{ name }}“ ist in diesem Ordner schon vorhanden.</p>
		<p v-if="canReplace" class="muted">Beim Ersetzen bleibt die bisherige Fassung als Version erhalten.</p>
		<label v-if="more" class="check"><input v-model="forAll" type="checkbox" /> Für alle weiteren übernehmen</label>
		<div class="stack choices">
			<button v-if="canReplace" class="primary" @click="emit('choose', 'replace', forAll)">Ersetzen</button>
			<button @click="emit('choose', 'keep_both', forAll)">Beide behalten</button>
			<button @click="emit('choose', 'skip', forAll)">Überspringen</button>
		</div>
	</Modal>
</template>

<style scoped>
.choices button { width: 100%; }
.check { display: flex; gap: 0.5rem; align-items: center; color: var(--text); }
</style>
