<script setup lang="ts">
// Asks for a name (new folder, rename) – head "Abbrechen · Titel · Aktion" as in the design. When
// renaming files, only the stem is selected ("Bericht" of "Bericht.pdf"), as Finder does.
const props = defineProps<{ title: string; initial?: string; action: string; hint?: string }>();
const emit = defineEmits<{ submit: [name: string]; cancel: [] }>();
const name = ref(props.initial ?? '');
const input = ref<HTMLInputElement | null>(null);

onMounted(() => {
	const el = input.value;
	if (!el) return;
	el.focus();
	const dot = name.value.lastIndexOf('.');
	el.setSelectionRange(0, dot > 0 ? dot : name.value.length);
});

function submit() {
	if (name.value.trim()) emit('submit', name.value.trim());
}
function clear() {
	name.value = '';
	input.value?.focus();
}
</script>

<template>
	<Modal :title="title" @cancel="emit('cancel')">
		<template #head="{ id }">
			<div class="bar">
				<button type="button" class="text" @click="emit('cancel')">Abbrechen</button>
				<h2 :id="id">{{ title }}</h2>
				<button type="submit" class="text strong" form="name-dialog" :disabled="!name.trim()">{{ action }}</button>
			</div>
		</template>
		<form id="name-dialog" @submit.prevent="submit">
			<label for="name-dialog-input" class="sr-only">Name</label>
			<div class="field">
				<input id="name-dialog-input" ref="input" v-model="name" autocomplete="off" required />
				<button v-if="name" type="button" class="icon" aria-label="Eingabe löschen" @click="clear">
					<Icon name="x" :size="15" :stroke="1.8" />
				</button>
			</div>
			<p v-if="hint" class="muted hint">{{ hint }}</p>
		</form>
	</Modal>
</template>

<style scoped>
.bar { display: grid; grid-template-columns: 1fr auto 1fr; align-items: center; gap: 8px; margin: 4px -10px 14px; }
.bar > :last-child { justify-self: end; }
.bar > :first-child { justify-self: start; }
.bar h2 { text-align: center; margin: 0; font-size: 15px; font-weight: 500; color: var(--ink); }
.bar .text { min-height: 44px; padding: 0 10px; font-size: 15px; }
.bar .strong { font-weight: 600; }
.field { position: relative; }
.field input { border-color: var(--ink); padding-right: 48px; }
.field .icon { position: absolute; right: 5px; top: 5px; width: 40px; height: 40px; color: var(--muted); }
.hint { margin: 10px 4px 0; font-size: 12.5px; }
</style>
