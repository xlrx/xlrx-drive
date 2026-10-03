<script setup lang="ts">
// "Neu" (the "+" of the design): upload files or photos, or create a folder – in the folder shown,
// otherwise in "Meine Ablage". Below: what is being uploaded.
const emit = defineEmits<{ close: [] }>();
const folder = useFolder();
const uploads = useUploads();
const target = ref<{ id: number; name: string } | null>(folder.value);
const naming = ref(false);
const error = ref('');
const files = ref<HTMLInputElement | null>(null);
const photos = ref<HTMLInputElement | null>(null);

onMounted(async () => {
	if (target.value) return;
	try {
		const home = (await apiGet<RootInfo[]>('/roots'))[0];
		if (home) target.value = { id: home.node_id, name: home.name };
	} catch (e) {
		error.value = errorMessage(e);
	}
});

function picked(e: Event) {
	const input = e.target as HTMLInputElement;
	if (input.files?.length && target.value) uploads.add(Array.from(input.files), target.value.id);
	input.value = '';
}

async function newFolder(name: string) {
	naming.value = false;
	if (!target.value) return;
	try {
		const created = await apiPost<NodeInfo>(`/nodes/${target.value.id}/folders`, { name });
		emit('close');
		await navigateTo(`/files/${created.id}`);
	} catch (e) {
		error.value = errorMessage(e);
	}
}

const running = computed(() => uploads.items.value.filter((i) => i.state !== 'done' && i.state !== 'skipped'));
const doneCount = computed(() => uploads.items.value.filter((i) => i.state === 'done').length);
</script>

<template>
	<Modal v-if="!naming" title="Neu" @cancel="emit('close')">
		<template #head="{ id }">
			<div class="head">
				<div>
					<h2 :id="id">Neu</h2>
					<span v-if="target" class="where">in <strong>{{ target.name }}</strong></span>
				</div>
				<button type="button" class="icon" aria-label="Schließen" @click="emit('close')">
					<span class="ring"><Icon name="x" :size="13" :stroke="2" /></span>
				</button>
			</div>
		</template>
		<div class="create">
			<button type="button" class="tile accent" :disabled="!target" @click="files?.click()">
				<Icon name="upload" :size="23" /><span>Dateien</span>
			</button>
			<button type="button" class="tile" :disabled="!target" @click="photos?.click()">
				<Icon name="image" :size="23" /><span>Fotos</span>
			</button>
			<button type="button" class="tile" :disabled="!target" @click="naming = true">
				<Icon name="folderPlus" :size="23" /><span>Ordner</span>
			</button>
		</div>
		<input ref="files" type="file" multiple hidden @change="picked" />
		<input ref="photos" type="file" multiple hidden accept="image/*,video/*" @change="picked" />
		<p v-if="error" class="error" role="alert">{{ error }}</p>
		<template v-if="uploads.items.value.length">
			<hr />
			<h3 class="section">Uploads · {{ doneCount }} von {{ uploads.items.value.length }}</h3>
			<ul class="ups">
				<li v-for="i in uploads.items.value" :key="i.key">
					<span class="line"><span class="name">{{ i.name }}</span><span class="pct mono">{{ i.state === 'done' ? 'fertig' : `${Math.round((100 * i.loaded) / Math.max(i.size, 1))} %` }}</span></span>
					<span class="bar"><span :style="{ width: `${(100 * (i.state === 'done' ? 1 : i.loaded / Math.max(i.size, 1))).toFixed(1)}%` }"></span></span>
					<span v-if="i.error" class="error small">{{ i.error }}</span>
				</li>
			</ul>
			<p v-if="running.length" class="muted small">Uploads laufen weiter, auch wenn du eine andere Seite öffnest.</p>
		</template>
	</Modal>
	<NameDialog v-else title="Neuer Ordner" action="Anlegen" @submit="newFolder" @cancel="naming = false" />
</template>

<style scoped>
.head { display: flex; align-items: center; justify-content: space-between; padding: 12px 0 14px; margin-right: -14px; }
.head h2 { margin: 0; font-size: 22px; font-weight: 400; letter-spacing: -0.02em; color: var(--ink); }
.where { font-size: 13px; color: var(--muted); }
.where strong { font-weight: 400; color: var(--ink); }
.ring { width: 30px; height: 30px; border-radius: 15px; border: 1px solid var(--line-strong); display: flex; align-items: center; justify-content: center; }
.create { display: grid; grid-template-columns: repeat(3, minmax(0, 1fr)); gap: 8px; padding-bottom: 16px; }
.tile { height: 82px; border-radius: 12px; flex-direction: column; gap: 8px; font-size: 12px; padding: 0; }
.tile.accent { background: var(--accent); border-color: var(--accent); color: var(--accent-text); }
.tile.accent:hover:not(:disabled) { background: color-mix(in srgb, var(--accent) 88%, var(--paper)); }
.section { font-size: 13px; font-weight: 400; color: var(--muted); margin: 12px 0 4px; }
.ups { list-style: none; margin: 0; padding: 0; }
.ups li { display: flex; flex-direction: column; gap: 6px; padding: 10px 0; border-bottom: 1px solid var(--line); }
.line { display: flex; justify-content: space-between; gap: 8px; align-items: baseline; }
.name { font-size: 14.5px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.pct { flex: none; font-size: 11px; color: var(--ink-3); }
.bar { height: 2px; background: var(--line); }
.bar span { display: block; height: 2px; background: var(--ink); }
.small { font-size: 12px; margin: 10px 0 0; }
</style>
