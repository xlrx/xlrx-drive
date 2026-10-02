<script setup lang="ts">
// Picks a target folder within the same root: browse folders, then "Hierher verschieben".
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
</script>

<template>
	<Modal :title="`„${node.name}“ verschieben`" @cancel="emit('cancel')">
		<nav v-if="here" class="crumbs" aria-label="Zielordner">
			<template v-for="(c, i) in here.path" :key="c.id">
				<span v-if="i" aria-hidden="true">›</span>
				<button class="link" type="button" @click="open(c.id)">{{ c.name }}</button>
			</template>
		</nav>
		<ul class="folders">
			<li v-for="f in folders" :key="f.id">
				<button
					type="button"
					class="folder"
					:disabled="f.id === node.id"
					@click="open(f.id)"
				>
					<FileIcon kind="folder" /> {{ f.name }}
				</button>
			</li>
			<li v-if="here && !folders.length" class="muted">Keine Unterordner.</li>
		</ul>
		<p v-if="error" class="error" role="alert">{{ error }}</p>
		<div class="row actions">
			<button type="button" @click="emit('cancel')">Abbrechen</button>
			<button class="primary" :disabled="!allowed" @click="here && emit('move', here.id)">
				Hierher verschieben
			</button>
		</div>
	</Modal>
</template>

<style scoped>
.crumbs { display: flex; flex-wrap: wrap; gap: 0.3rem; align-items: center; margin-bottom: 0.5rem; }
.link { border: 0; background: none; padding: 0.2rem; color: var(--accent); }
.folders { list-style: none; margin: 0; padding: 0; border: 1px solid var(--border); border-radius: 8px; max-height: 18rem; overflow: auto; }
.folders li + li { border-top: 1px solid var(--border); }
.folders li.muted { padding: 0.7rem; }
.folder { width: 100%; text-align: left; border: 0; border-radius: 0; display: flex; gap: 0.6rem; align-items: center; }
.actions { justify-content: flex-end; margin-top: 1.2rem; }
</style>
