<script setup lang="ts">
// A folder (list of its contents) or a file (preview and download).
const route = useRoute();
const id = computed(() => Number(route.params.id));

const node = ref<NodeDetail | null>(null);
const children = ref<NodeInfo[]>([]);
const root = ref<RootInfo | null>(null);
const loading = ref(true);
const busy = ref(false);
const error = ref('');
const notice = ref('');
let poll: ReturnType<typeof setTimeout> | undefined;

const title = computed(() => node.value?.path.at(-1)?.name ?? '');
useHead({ title: computed(() => (title.value ? `${title.value} – xlrx drive` : 'xlrx drive')) });

async function load() {
	clearTimeout(poll);
	try {
		const n = await apiGet<NodeDetail>(`/nodes/${id.value}`);
		const [kids, roots] = await Promise.all([
			n.kind === 'dir' ? apiGet<NodeInfo[]>(`/nodes/${n.id}/children`) : Promise.resolve([]),
			apiGet<RootInfo[]>('/roots')
		]);
		node.value = n;
		children.value = kids;
		root.value = roots.find((r) => r.id === n.root_id) ?? null;
		error.value = '';
		// First import still running: show what is there and look again shortly.
		if (root.value && !root.value.scanned_at) poll = setTimeout(load, 1500);
	} catch (e) {
		node.value = null;
		error.value =
			e instanceof ApiError && e.status === 404
				? 'Nicht gefunden – vielleicht gelöscht oder verschoben.'
				: errorMessage(e);
	} finally {
		loading.value = false;
	}
}

watch(
	id,
	() => {
		loading.value = true;
		notice.value = '';
		load();
	},
	{ immediate: true }
);
onBeforeUnmount(() => clearTimeout(poll));

async function rescan() {
	if (!node.value) return;
	busy.value = true;
	notice.value = '';
	try {
		notice.value = describeScan(await apiPost<ScanReport>(`/roots/${node.value.root_id}/scan`));
		await load();
	} catch (e) {
		error.value = errorMessage(e);
	} finally {
		busy.value = false;
	}
}
</script>

<template>
	<main class="page">
		<nav v-if="node && node.path.length > 1" class="crumbs" aria-label="Pfad">
			<template v-for="(c, i) in node.path.slice(0, -1)" :key="c.id">
				<span v-if="i" aria-hidden="true">›</span>
				<NuxtLink :to="`/files/${c.id}`">{{ c.name }}</NuxtLink>
			</template>
		</nav>
		<div class="row head">
			<h1>{{ title }}</h1>
			<span class="spacer"></span>
			<button v-if="node?.kind === 'dir'" :disabled="busy" @click="rescan">
				{{ busy ? 'Liest ein …' : 'Neu einlesen' }}
			</button>
			<template v-else-if="node">
				<a
					v-if="previewKind(node.mime) && previewKind(node.mime) !== 'office'"
					class="button"
					:href="contentUrl(node.id, true)"
					target="_blank"
					rel="noopener"
					>In neuem Tab</a
				>
				<a class="button primary" :href="contentUrl(node.id)" download>Herunterladen</a>
			</template>
		</div>
		<p v-if="node?.kind === 'file'" class="muted meta">
			{{ formatSize(node.size) }} · geändert {{ formatDate(node.mtime) }}
		</p>
		<p v-if="error" class="error" role="alert">{{ error }}</p>
		<p v-if="notice" class="muted" role="status">{{ notice }}</p>
		<p v-if="root && !root.scanned_at" class="muted" role="status">
			Die Ablage wird zum ersten Mal eingelesen – Inhalte erscheinen nach und nach.
		</p>

		<div v-if="node?.kind === 'dir'" class="card list">
			<table v-if="children.length">
				<thead>
					<tr>
						<th>Name</th>
						<th class="when">Geändert</th>
						<th class="size">Größe</th>
						<th class="act"><span class="sr-only">Herunterladen</span></th>
					</tr>
				</thead>
				<tbody>
					<tr v-for="c in children" :key="c.id">
						<td>
							<NuxtLink :to="`/files/${c.id}`" class="name">
								<FileIcon :kind="iconKind(c)" />
								<span>{{ c.name }}</span>
							</NuxtLink>
						</td>
						<td class="when muted">{{ formatShortDate(c.mtime) }}</td>
						<td class="size muted">{{ c.kind === 'file' ? formatSize(c.size) : '' }}</td>
						<td class="act">
							<a
								v-if="c.kind === 'file'"
								:href="contentUrl(c.id)"
								download
								:aria-label="`${c.name} herunterladen`"
								title="Herunterladen"
								>↓</a
							>
						</td>
					</tr>
				</tbody>
			</table>
			<p v-else-if="!loading" class="muted empty">Dieser Ordner ist leer.</p>
		</div>
		<FilePreview v-else-if="node" :node="node" />
	</main>
</template>

<style scoped>
.crumbs { display: flex; flex-wrap: wrap; gap: 0.4rem; font-size: 0.9rem; color: var(--muted); margin-bottom: 0.4rem; }
.crumbs a { color: var(--muted); text-decoration: none; }
.crumbs a:hover { color: var(--accent); text-decoration: underline; }
.head h1 { margin: 0; overflow-wrap: anywhere; }
.spacer { flex: 1; }
.meta { margin: 0.25rem 0 1rem; }
.head + p, .meta + p { margin-top: 0.5rem; }
.list { padding: 0.25rem 0; margin-top: 1rem; }
.list table td, .list table th { padding-left: 1rem; padding-right: 1rem; }
.list tbody tr:last-child td { border-bottom: 0; }
.list tbody tr:hover { background: var(--bg); }
.name { display: flex; align-items: center; gap: 0.6rem; color: var(--text); text-decoration: none; overflow-wrap: anywhere; }
.name:hover span { text-decoration: underline; }
.when { white-space: nowrap; width: 9rem; }
.size { white-space: nowrap; width: 6rem; text-align: right; }
.act { width: 2.5rem; text-align: center; }
.act a { text-decoration: none; font-size: 1.1rem; }
.empty { padding: 1.5rem 1rem; margin: 0; }
.preview { margin-top: 1rem; }
@media (max-width: 40rem) {
	.when { display: none; }
}
</style>
