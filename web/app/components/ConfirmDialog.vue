<script setup lang="ts">
// A question before an action (as "In den Papierkorb verschieben?" in the design): what it is
// about, what happens, then the action and "Abbrechen".
const props = defineProps<{ title: string; text: string; action: string; node?: Pick<NodeInfo, 'id' | 'kind' | 'name' | 'mime' | 'rev'>; danger?: boolean }>();
const emit = defineEmits<{ confirm: []; cancel: [] }>();
const button = ref<HTMLButtonElement | null>(null);
onMounted(() => button.value?.focus());
</script>

<template>
	<Modal :title="title" role="alertdialog" @cancel="emit('cancel')">
		<template #head="{ id }">
			<div class="head">
				<FileMark v-if="props.node" :node="props.node" big />
				<div>
					<h2 :id="id">{{ title }}</h2>
					<p>{{ text }}</p>
				</div>
			</div>
		</template>
		<hr />
		<slot />
		<div class="actions">
			<button ref="button" class="big" :class="danger ? 'danger solid' : 'primary'" @click="emit('confirm')">{{ action }}</button>
			<button class="big" @click="emit('cancel')">Abbrechen</button>
		</div>
	</Modal>
</template>

<style scoped>
.head { display: flex; gap: 16px; align-items: flex-start; padding: 16px 0; }
.head h2 { margin: 0; font-size: 21px; line-height: 1.2; font-weight: 400; letter-spacing: -0.02em; color: var(--ink); }
.head p { margin: 6px 0 0; font-size: 14px; color: var(--ink-3); }
.actions { display: flex; flex-direction: column; gap: 8px; padding-top: 16px; }
.actions button { width: 100%; }
</style>
