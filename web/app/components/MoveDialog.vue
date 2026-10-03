<script setup lang="ts">
// Picks a target folder within the same root (as in the design): open folders along the path,
// then move here.
const props = defineProps<{ node: NodeInfo; rootNode: number }>();
const emit = defineEmits<{ move: [parentId: number]; cancel: [] }>();
const here = ref<NodeDetail | null>(null);
const folders = ref<NodeInfo[]>([]);
const error = ref('');

async function open(id: number) {
	try {
		here.value = await apiGet<NodeDetail>(`/nodes/${id}`);
		folders.value = (await apiGet<NodeInfo[]>(`/nodes/${id}/children`)).filter((n) => n.kind === 'dir');
		error.value = '';
	} catch (e) {
		error.value = errorMessage(e);
	}
}
onMounted(() => open(props.node.parent_id ?? props.rootNode));

/** Not into its current folder, not into itself or below. */
const allowed = computed(
	() =>
		!!here.value &&
		here.value.id !== props.node.parent_id &&
		!here.value.path.some((c) => c.id === props.node.id)
);
const target = computed(() => here.value?.path.at(-1)?.name ?? '');
</script>

<template>
	<Modal title="Verschieben" wide @cancel="emit('cancel')">
		<template #head="{ id }">
			<div class="bar">
				<button type="button" class="text" @click="emit('cancel')">Abbrechen</button>
				<h2 :id="id">Verschieben</h2>
				<span class="slot"></span>
			</div>
		</template>
		<p class="what"><FileMark :node="node" /><span>{{ node.name }}</span></p>
		<nav v-if="here" class="crumbs" aria-label="Zielordner">
			<template v-for="(c, i) in here.path" :key="c.id">
				<Icon v-if="i" name="chevR" :size="14" :stroke="1.8" />
				<button v-if="i < here.path.length - 1" class="link" type="button" @click="open(c.id)">{{ c.name }}</button>
				<span v-else class="current">{{ c.name }}</span>
			</template>
		</nav>
		<ul class="rows folders">
			<li v-for="f in folders" :key="f.id">
				<button type="button" class="folder" :disabled="f.id === node.id" @click="open(f.id)">
					<Icon name="folder" :size="25" :stroke="1.3" />
					<span class="name">{{ f.name }}</span>
					<Icon name="chevR" :size="17" :stroke="1.6" class="go" />
				</button>
			</li>
			<li v-if="here && !folders.length" class="muted empty">Keine Unterordner.</li>
		</ul>
		<p v-if="error" class="error" role="alert">{{ error }}</p>
		<button class="primary big full" :disabled="!allowed" @click="here && emit('move', here.id)">
			{{ allowed ? `In „${target}“ verschieben` : 'Hierher verschieben' }}
		</button>
	</Modal>
</template>

<style scoped>
.bar { display: grid; grid-template-columns: 1fr auto 1fr; align-items: center; margin: 4px -10px 6px; }
.bar > :last-child { justify-self: end; }
.bar > :first-child { justify-self: start; }
.bar h2 { text-align: center; margin: 0; font-size: 15px; font-weight: 500; color: var(--ink); }
.bar .text { min-height: 44px; padding: 0 10px; font-size: 15px; }
.slot { width: 90px; }
.what { display: flex; align-items: center; gap: 10px; margin: 0 0 10px; font-size: 13px; color: var(--muted); overflow: hidden; }
.what span:last-child { white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.what :deep(.mark) { transform: scale(0.7); margin: -6px; }
.crumbs { display: flex; flex-wrap: wrap; align-items: center; gap: 2px; font-size: 13px; color: var(--muted); padding: 6px 0; border-top: 1px solid var(--line); }
.crumbs .link { min-height: 32px; padding: 0 4px; font-size: 13px; color: var(--muted); }
.crumbs .current { padding: 0 4px; color: var(--ink); }
.folders { max-height: 18rem; overflow: auto; }
.folders li { min-height: 56px; }
.folder { width: 100%; min-height: 56px; border: 0; border-radius: 0; padding: 0 4px; justify-content: flex-start; gap: 14px; }
.folder .name { flex: 1; text-align: left; }
.folder .go { color: var(--faint); }
.empty { padding: 0 4px; font-size: 14px; }
</style>
