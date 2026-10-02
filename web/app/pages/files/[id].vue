<script setup lang="ts">
// A folder (contents, upload, new folder) or a file (preview, download, versions). Rename, move
// and delete for both; deleted items go to the trash and can be brought back.
const route = useRoute();
const id = computed(() => Number(route.params.id));

const node = ref<NodeDetail | null>(null);
const children = ref<NodeInfo[]>([]);
const versions = ref<VersionInfo[]>([]);
const root = ref<RootInfo | null>(null);
const loading = ref(true);
const busy = ref(false);
const error = ref('');
const notice = ref('');
/** Last deleted item, for "Rückgängig". */
const undo = ref<NodeInfo | null>(null);
const dragging = ref(false);
let poll: ReturnType<typeof setTimeout> | undefined;

type Dialog =
	| { kind: 'folder' }
	| { kind: 'rename'; node: NodeInfo }
	| { kind: 'move'; node: NodeInfo };
const dialog = ref<Dialog | null>(null);

const title = computed(() => node.value?.path.at(-1)?.name ?? '');
useHead({ title: computed(() => (title.value ? `${title.value} – xlrx drive` : 'xlrx drive')) });

async function load() {
	clearTimeout(poll);
	try {
		const n = await apiGet<NodeDetail>(`/nodes/${id.value}`);
		const [kids, vers, roots] = await Promise.all([
			n.kind === 'dir' ? apiGet<NodeInfo[]>(`/nodes/${n.id}/children`) : Promise.resolve([]),
			n.kind === 'file' ? apiGet<VersionInfo[]>(`/nodes/${n.id}/versions`) : Promise.resolve([]),
			apiGet<RootInfo[]>('/roots')
		]);
		node.value = n;
		children.value = kids;
		versions.value = vers;
		root.value = roots.find((r) => r.id === n.root_id) ?? null;
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
		error.value = '';
		load();
	},
	{ immediate: true }
);
onBeforeUnmount(() => clearTimeout(poll));

/** Runs a change; on failure shows the reason and reloads (the server may have rescanned). */
async function act(fn: () => Promise<void>) {
	busy.value = true;
	error.value = '';
	notice.value = '';
	undo.value = null;
	try {
		await fn();
	} catch (e) {
		error.value = errorMessage(e);
	} finally {
		busy.value = false;
		await load();
	}
}

const rescan = () =>
	act(async () => {
		notice.value = describeScan(await apiPost<ScanReport>(`/roots/${node.value!.root_id}/scan`));
	});

const newFolder = (name: string) =>
	act(async () => {
		dialog.value = null;
		await apiPost(`/nodes/${id.value}/folders`, { name });
	});

const rename = (n: NodeInfo, name: string) =>
	act(async () => {
		dialog.value = null;
		await apiPatch(`/nodes/${n.id}`, { name, if_seq: n.seq });
	});

const move = (n: NodeInfo, parentId: number) =>
	act(async () => {
		dialog.value = null;
		await apiPatch(`/nodes/${n.id}`, { parent_id: parentId, if_seq: n.seq });
		notice.value = `„${n.name}“ wurde verschoben.`;
	});

async function remove(n: NodeInfo) {
	const viewing = n.id === node.value?.id;
	const parent = n.parent_id;
	await act(async () => {
		await apiDelete(`/nodes/${n.id}?if_seq=${n.seq}`);
		if (viewing && parent) await navigateTo(`/files/${parent}`);
		notice.value = `„${n.name}“ liegt jetzt im Papierkorb.`;
		undo.value = n;
	});
}

async function restoreDeleted() {
	const n = undo.value;
	if (!n) return;
	await act(async () => {
		await apiPost(`/trash/${n.id}/restore`);
		notice.value = `„${n.name}“ ist wieder da.`;
	});
}

const restoreVersion = (v: VersionInfo) =>
	act(async () => {
		await apiPost(`/versions/${v.id}/restore`);
		notice.value = 'Die Fassung ist wiederhergestellt; die bisherige bleibt als Version erhalten.';
	});

// Uploads into the folder shown, or a new version of the file shown.
const uploads = useUploads(() => load());
const picker = ref<HTMLInputElement | null>(null);
const versionPicker = ref<HTMLInputElement | null>(null);

function picked(e: Event) {
	const input = e.target as HTMLInputElement;
	if (input.files?.length && node.value) uploads.add(Array.from(input.files), node.value.id);
	input.value = '';
}

function dropped(e: DragEvent) {
	dragging.value = false;
	if (node.value?.kind !== 'dir') return;
	const entries = Array.from(e.dataTransfer?.items ?? []).filter((i) => i.kind === 'file');
	// Folders arrive as entries too; uploading whole folders comes later.
	const files = entries
		.filter((i) => i.webkitGetAsEntry?.()?.isFile ?? true)
		.map((i) => i.getAsFile())
		.filter((f): f is File => !!f);
	if (files.length < entries.length) notice.value = 'Ordner lassen sich noch nicht hochladen – nur Dateien.';
	if (files.length) uploads.add(files, node.value.id);
}

async function newVersion(e: Event) {
	const input = e.target as HTMLInputElement;
	const file = input.files?.[0];
	input.value = '';
	const n = node.value;
	if (!file || !n) return;
	await act(async () => {
		await sendFile('PUT', `/nodes/${n.id}/content?${uploadQuery(file, { base_rev: String(n.rev) })}`, file);
		notice.value = 'Neue Fassung gespeichert; die bisherige bleibt als Version erhalten.';
	});
}
</script>

<template>
	<main
		class="page"
		@dragover.prevent="dragging = node?.kind === 'dir'"
		@dragleave.self="dragging = false"
		@drop.prevent="dropped"
	>
		<nav v-if="node && node.path.length > 1" class="crumbs" aria-label="Pfad">
			<template v-for="(c, i) in node.path.slice(0, -1)" :key="c.id">
				<span v-if="i" aria-hidden="true">›</span>
				<NuxtLink :to="`/files/${c.id}`">{{ c.name }}</NuxtLink>
			</template>
		</nav>
		<div class="row head">
			<h1>{{ title }}</h1>
			<span class="spacer"></span>
			<template v-if="node?.kind === 'dir'">
				<button :disabled="busy" @click="dialog = { kind: 'folder' }">Neuer Ordner</button>
				<button class="primary" :disabled="busy" @click="picker?.click()">Hochladen</button>
				<input ref="picker" type="file" multiple hidden data-testid="upload-input" @change="picked" />
				<button :disabled="busy" @click="rescan">{{ busy ? 'Bitte warten …' : 'Neu einlesen' }}</button>
			</template>
			<template v-else-if="node">
				<a
					v-if="opensInBrowser(node.mime)"
					class="button"
					:href="contentUrl(node.id, true)"
					target="_blank"
					rel="noopener"
					>In neuem Tab</a
				>
				<a class="button primary" :href="contentUrl(node.id)" download>Herunterladen</a>
				<RowMenu
					:label="node.name"
					@rename="dialog = { kind: 'rename', node }"
					@move="dialog = { kind: 'move', node }"
					@remove="remove(node)"
				/>
			</template>
		</div>
		<p v-if="node?.kind === 'file'" class="muted meta">
			{{ formatSize(node.size) }} · geändert {{ formatDate(node.mtime) }}
		</p>
		<p v-if="error" class="error" role="alert">{{ error }}</p>
		<p v-if="notice" class="notice" role="status">
			{{ notice }}
			<button v-if="undo" class="link" type="button" @click="restoreDeleted">Rückgängig</button>
		</p>
		<p v-if="root && !root.scanned_at" class="muted" role="status">
			Die Ablage wird zum ersten Mal eingelesen – Inhalte erscheinen nach und nach.
		</p>

		<div v-if="node?.kind === 'dir'" class="card list" :class="{ dragging }">
			<table v-if="children.length">
				<thead>
					<tr>
						<th>Name</th>
						<th class="when">Geändert</th>
						<th class="size">Größe</th>
						<th class="act"><span class="sr-only">Aktionen</span></th>
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
								class="download"
								:href="contentUrl(c.id)"
								download
								:aria-label="`${c.name} herunterladen`"
								title="Herunterladen"
								>↓</a
							>
							<RowMenu
								:label="c.name"
								@rename="dialog = { kind: 'rename', node: c }"
								@move="dialog = { kind: 'move', node: c }"
								@remove="remove(c)"
							/>
						</td>
					</tr>
				</tbody>
			</table>
			<p v-else-if="!loading" class="muted empty">
				Dieser Ordner ist leer. Dateien hierher ziehen oder „Hochladen“ wählen.
			</p>
			<p v-if="dragging" class="drop">Loslassen zum Hochladen</p>
		</div>

		<template v-else-if="node">
			<FilePreview :node="node" />
			<section class="versions">
				<div class="row">
					<h2>Versionen</h2>
					<span class="spacer"></span>
					<button :disabled="busy" @click="versionPicker?.click()">Neue Fassung hochladen</button>
					<input ref="versionPicker" type="file" hidden data-testid="version-input" @change="newVersion" />
				</div>
				<div class="card list">
					<table v-if="versions.length">
						<thead>
							<tr>
								<th>Fassung vom</th>
								<th class="size">Größe</th>
								<th>Ersetzt von</th>
								<th class="act"><span class="sr-only">Aktionen</span></th>
							</tr>
						</thead>
						<tbody>
							<tr v-for="v in versions" :key="v.id">
								<td>{{ formatDate(v.mtime ?? v.created_at) }}</td>
								<td class="size muted">{{ formatSize(v.size) }}</td>
								<td class="muted">{{ v.created_by ?? 'außerhalb von xlrx' }}</td>
								<td class="act wide">
									<a :href="`/api/versions/${v.id}/content`" download>Herunterladen</a>
									<button class="link" type="button" :disabled="busy" @click="restoreVersion(v)">
										Wiederherstellen
									</button>
								</td>
							</tr>
						</tbody>
					</table>
					<p v-else class="muted empty">
						Noch keine früheren Fassungen. Wird die Datei über xlrx ersetzt, bleibt die bisherige hier erhalten.
					</p>
				</div>
			</section>
		</template>

		<NameDialog
			v-if="dialog?.kind === 'folder'"
			title="Neuer Ordner"
			action="Anlegen"
			@submit="newFolder"
			@cancel="dialog = null"
		/>
		<NameDialog
			v-else-if="dialog?.kind === 'rename'"
			title="Umbenennen"
			action="Umbenennen"
			:initial="dialog.node.name"
			@submit="(name) => dialog?.kind === 'rename' && rename(dialog.node, name)"
			@cancel="dialog = null"
		/>
		<MoveDialog
			v-else-if="dialog?.kind === 'move' && root"
			:node="dialog.node"
			:root-node="root.node_id"
			@move="(p) => dialog?.kind === 'move' && move(dialog.node, p)"
			@cancel="dialog = null"
		/>
		<ConflictDialog
			v-if="uploads.question.value"
			:name="uploads.question.value.name"
			:can-replace="uploads.question.value.canReplace"
			:more="uploads.question.value.more"
			@choose="(c, all) => uploads.question.value?.answer(c, all)"
		/>
		<UploadQueue :items="uploads.items.value" :active="uploads.active.value" @close="uploads.clear()" />
	</main>
</template>

<style scoped>
.crumbs { display: flex; flex-wrap: wrap; gap: 0.4rem; font-size: 0.9rem; color: var(--muted); margin-bottom: 0.4rem; }
.crumbs a { color: var(--muted); text-decoration: none; }
.crumbs a:hover { color: var(--accent); text-decoration: underline; }
.head h1 { margin: 0; overflow-wrap: anywhere; }
.spacer { flex: 1; }
.meta { margin: 0.25rem 0 1rem; }
.notice { margin: 0.75rem 0 0; }
.link { border: 0; background: none; padding: 0 0.3rem; color: var(--accent); text-decoration: underline; }
.list { padding: 0.25rem 0; margin-top: 1rem; position: relative; }
.list.dragging { outline: 2px dashed var(--accent); outline-offset: 4px; }
.drop { position: absolute; inset: 0; display: grid; place-items: center; margin: 0; background: color-mix(in srgb, var(--surface) 85%, transparent); color: var(--accent); font-weight: 600; border-radius: var(--radius); }
.list table td, .list table th { padding-left: 1rem; padding-right: 1rem; }
.list tbody tr:last-child td { border-bottom: 0; }
.list tbody tr:hover { background: var(--bg); }
.name { display: flex; align-items: center; gap: 0.6rem; color: var(--text); text-decoration: none; overflow-wrap: anywhere; }
.name:hover span { text-decoration: underline; }
.when { white-space: nowrap; width: 9rem; }
.size { white-space: nowrap; width: 6rem; text-align: right; }
.act { width: 5rem; text-align: right; white-space: nowrap; }
.act.wide { width: auto; }
.act .download { text-decoration: none; font-size: 1.1rem; padding: 0 0.3rem; }
.empty { padding: 1.5rem 1rem; margin: 0; }
.preview { margin-top: 1rem; }
.versions { margin-top: 2rem; }
.versions h2 { margin: 0; }
@media (max-width: 40rem) {
	.when { display: none; }
	.head button, .head .button { padding: 0.5rem 0.7rem; }
}
</style>
