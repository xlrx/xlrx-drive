<script setup lang="ts">
// Asks for a name (new folder, rename). The extension stays selected-out when renaming files.
const props = defineProps<{ title: string; initial?: string; action: string }>();
const emit = defineEmits<{ submit: [name: string]; cancel: [] }>();
const name = ref(props.initial ?? '');
const input = ref<HTMLInputElement | null>(null);

onMounted(() => {
	const el = input.value;
	if (!el) return;
	el.focus();
	// Select the stem only ("Bericht" of "Bericht.pdf"), as Finder does.
	const dot = name.value.lastIndexOf('.');
	el.setSelectionRange(0, dot > 0 ? dot : name.value.length);
});
</script>

<template>
	<Modal :title="title" @cancel="emit('cancel')">
		<form @submit.prevent="name.trim() && emit('submit', name.trim())">
			<label for="name-dialog-input">Name</label>
			<input id="name-dialog-input" ref="input" v-model="name" autocomplete="off" required />
			<div class="row actions">
				<button type="button" @click="emit('cancel')">Abbrechen</button>
				<button class="primary" :disabled="!name.trim()">{{ action }}</button>
			</div>
		</form>
	</Modal>
</template>

<style scoped>
.actions { justify-content: flex-end; margin-top: 1.2rem; }
</style>
