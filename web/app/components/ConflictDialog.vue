<script setup lang="ts">
// A file of that name exists already: replace (the old content stays as a version), keep both,
// or skip. Optionally for all remaining conflicts of this upload.
defineProps<{ name: string; canReplace: boolean; more: boolean }>();
const emit = defineEmits<{ choose: [choice: ConflictChoice, forAll: boolean] }>();
const forAll = ref(false);
</script>

<template>
	<Modal title="Datei gibt es schon" role="alertdialog" @cancel="emit('choose', 'skip', false)">
		<template #head="{ id }">
			<div class="head">
				<h2 :id="id">Datei gibt es schon</h2>
				<p>„{{ name }}“ ist in diesem Ordner schon vorhanden.</p>
				<p v-if="canReplace" class="muted">Beim Ersetzen bleibt die bisherige Fassung als Version erhalten.</p>
			</div>
		</template>
		<hr />
		<label v-if="more" class="check"><input v-model="forAll" type="checkbox" /> Für alle weiteren übernehmen</label>
		<div class="actions">
			<button v-if="canReplace" class="primary big" @click="emit('choose', 'replace', forAll)">Ersetzen</button>
			<button class="big" @click="emit('choose', 'keep_both', forAll)">Beide behalten</button>
			<button class="big" @click="emit('choose', 'skip', forAll)">Überspringen</button>
		</div>
	</Modal>
</template>

<style scoped>
.head { padding: 16px 0; }
.head h2 { margin: 0; font-size: 21px; line-height: 1.2; font-weight: 400; letter-spacing: -0.02em; color: var(--ink); }
.head p { margin: 6px 0 0; font-size: 14px; color: var(--ink-3); }
.head p.muted { color: var(--muted); font-size: 13px; }
.check { margin: 14px 0 0; }
.actions { display: flex; flex-direction: column; gap: 8px; padding-top: 16px; }
</style>
