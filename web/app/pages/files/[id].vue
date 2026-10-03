<script setup lang="ts">
// A folder (contents, upload, new folder) or a file (preview, info, versions), as in the design
// screens "Ablage", "Vorschau" and "Details". Rename, move and delete for both; deleted items go to
// the trash and can be brought back.
const route = useRoute();
const id = computed(() => Number(route.params.id));
const folder = useFolder();
const uploads = useUploads();

const node = ref<NodeDetail | null>(null);
const children = ref<ChildInfo[]>([]);
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
let noted = 0;

type Dialog =
	| { kind: 'folder' }
	| { kind: 'rename'; node: NodeInfo }
	| { kind: 'move'; node: NodeInfo }
	| { kind: 'remove'; node: NodeInfo }
	| { kind: 'share'; node: NodeInfo }
	| { kind: 'class'; id: number };
const dialog = ref<Dialog | null>(null);

const title = computed(() => node.value?.path.at(-1)?.name ?? '');
/** May change things here (viewers only look). */
const editable = computed(() => canEdit(node.value?.role));
/** Who has access (folder line "geteilt mit …", tab "Zugriff"). */
const access = ref<AccessInfo | null>(null);
const sharedWith = computed(() => {
	const names = (access.value?.shares ?? []).filter((s) => !s.expired).map((s) => s.to.name);
	return [...new Set(names)];
});
useHead({ title: computed(() => (title.value ? `${title.value} – xlrx drive` : 'xlrx drive')) });

// How folders are shown (kept in this browser).
type View = { sort: 'name' | 'mtime'; layout: 'list' | 'grid' };
const view = ref<View>({ sort: 'name', layout: 'list' });
onMounted(() => {
	try {
		view.value = { ...view.value, ...JSON.parse(localStorage.getItem('xlrx-folder-view') ?? '{}') };
	} catch {
		// keep the defaults
	}
});
watch(view, (v) => {
	try {
		localStorage.setItem('xlrx-folder-view', JSON.stringify(v));
	} catch {
		// storage blocked
	}
}, { deep: true });
const collator = new Intl.Collator('de', { numeric: true, sensitivity: 'base' });
const sorted = computed(() => {
	const list = [...children.value];
	if (view.value.sort === 'mtime') return list.sort((a, b) => (b.mtime ?? '').localeCompare(a.mtime ?? ''));
	return list.sort((a, b) => (a.kind === b.kind ? collator.compare(a.name, b.name) : a.kind === 'dir' ? -1 : 1));
});
const count = computed(() => (children.value.length === 1 ? '1 Element' : `${children.value.length} Elemente`));

// File view: tabs (the actions menu links to the versions directly).
type Tab = 'info' | 'versionen' | 'zugriff' | 'aktivitaet';
const tabFrom = (q: unknown): Tab => (q === 'versionen' || q === 'zugriff' || q === 'aktivitaet' ? q : 'info');
const tab = ref<Tab>(tabFrom(route.query.tab));
watch(id, () => (tab.value = tabFrom(route.query.tab)));
/** History of the file (tab "Aktivität"), loaded when shown. */
const history = ref<ActivityGroup[] | null>(null);
watch([tab, () => node.value?.id, () => node.value?.rev], async ([t]) => {
	if (t !== 'aktivitaet' || !node.value) return;
	try {
		history.value = (await apiGet<ActivityPage>(`/activity?node=${node.value.id}&limit=200`)).groups;
	} catch {
		history.value = [];
	}
});

async function load() {
	clearTimeout(poll);
	try {
		const n = await apiGet<NodeDetail>(`/nodes/${id.value}`);
		const [kids, vers, roots, acc] = await Promise.all([
			n.kind === 'dir' ? apiGet<ChildInfo[]>(`/nodes/${n.id}/children`) : Promise.resolve([]),
			n.kind === 'file' ? apiGet<VersionInfo[]>(`/nodes/${n.id}/versions`) : Promise.resolve([]),
			apiGet<RootInfo[]>('/roots'),
			apiGet<AccessInfo>(`/nodes/${n.id}/shares`).catch(() => null)
		]);
		node.value = n;
		children.value = kids;
		// Opening a file is noted for the person's start page (once, not on every refresh).
		if (n.kind === 'file' && noted !== n.id) {
			noted = n.id;
			apiPost(`/nodes/${n.id}/opened`, {}).catch(() => {});
		}
		versions.value = vers;
		access.value = acc;
		root.value = roots.find((r) => r.id === n.root_id) ?? null;
		// Target of "Neu": this folder, or the folder of this file – if I may add there.
		const where = n.kind === 'dir' ? n.path.at(-1) : n.path.at(-2);
		folder.value = where && canEdit(n.role) ? { id: where.id, name: where.name } : null;
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
onBeforeUnmount(() => {
	clearTimeout(poll);
	folder.value = null;
});
// Changes elsewhere (other devices, SMB, the watcher) and finished uploads: show them right away.
useLive().onRootChange(
	() => node.value?.root_id,
	() => {
		if (!busy.value) load();
	}
);
watch(() => uploads.finished.value, () => !busy.value && load());

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
	dialog.value = null;
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
const picker = ref<HTMLInputElement | null>(null);
const versionPicker = ref<HTMLInputElement | null>(null);

function picked(e: Event) {
	const input = e.target as HTMLInputElement;
	if (input.files?.length && node.value) uploads.add(Array.from(input.files), node.value.id);
	input.value = '';
}

function dropped(e: DragEvent) {
	dragging.value = false;
	if (node.value?.kind !== 'dir' || !editable.value) return;
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
		await uploadFile(file, { kind: 'replace', node_id: n.id, base_rev: n.rev });
		notice.value = 'Neue Fassung gespeichert; die bisherige bleibt als Version erhalten.';
	});
}

const meta = (c: NodeInfo) =>
	c.kind === 'file' ? [formatShortDate(c.mtime), formatSize(c.size)].filter(Boolean).join(' · ') : formatShortDate(c.mtime) || 'Ordner';
const extOf = (name: string) => {
	const dot = name.lastIndexOf('.');
	return dot > 0 ? name.slice(dot + 1).toUpperCase() : 'Datei';
};
const place = computed(() => node.value?.path.slice(0, -1).map((c) => c.name).join(' › ') ?? '');
</script>

<template>
	<main
		class="page"
		@dragover.prevent="dragging = node?.kind === 'dir' && editable"
		@dragleave.self="dragging = false"
		@drop.prevent="dropped"
	>
		<nav v-if="node && (node.path.length > 1 || node.shared)" class="crumbs" aria-label="Pfad">
			<NuxtLink v-if="node.shared" to="/shared">Geteilt</NuxtLink>
			<template v-for="(c, i) in node.path.slice(0, -1)" :key="c.id">
				<Icon v-if="i || node.shared" name="chevR" :size="14" :stroke="1.8" />
				<NuxtLink :to="`/files/${c.id}`">{{ c.name }}</NuxtLink>
			</template>
		</nav>

		<!-- Folder -->
		<template v-if="node?.kind === 'dir'">
			<div class="title-row">
				<div class="titles">
					<h1>{{ title }}</h1>
					<p class="meta">
						{{ count }}<template v-if="root"> · {{ root.name }}</template>
						<template v-else-if="access?.owner"> · von {{ access.owner }}</template>
						<template v-if="sharedWith.length"> · geteilt mit {{ sharedWith.join(', ') }}</template>
						<template v-if="!editable"> · nur ansehen</template>
						·
						<button type="button" class="link class-link" :aria-label="`Datenklasse: ${CLASS_LABEL[node.data_class.class]}`" @click="dialog = { kind: 'class', id: node.id }">
							<Icon :name="node.data_class.class === 'local' ? 'lock' : 'cloud'" :size="13" :stroke="1.8" />{{ CLASS_LABEL[node.data_class.class] }}
						</button>
					</p>
				</div>
				<div class="tools">
					<button :disabled="busy" class="desktop" @click="dialog = { kind: 'share', node }"><Icon name="shared" :size="18" />Teilen</button>
					<button v-if="editable" :disabled="busy" class="desktop" @click="dialog = { kind: 'folder' }"><Icon name="folderPlus" :size="18" />Neuer Ordner</button>
					<button v-if="editable" class="primary desktop" :disabled="busy" @click="picker?.click()"><Icon name="upload" :size="18" />Hochladen</button>
					<input ref="picker" type="file" multiple hidden data-testid="upload-input" @change="picked" />
				</div>
			</div>
		</template>

		<!-- File -->
		<template v-else-if="node">
			<h1 class="file-title">{{ title }}</h1>
			<p class="label">{{ extOf(node.name) }} · {{ formatSize(node.size) }} · geändert {{ formatShortDate(node.mtime) }}</p>
			<div class="row file-actions">
				<a class="button primary" :href="contentUrl(node.id)" download><Icon name="download" :size="18" />Herunterladen</a>
				<a v-if="opensInBrowser(node.mime)" class="button" :href="contentUrl(node.id, true)" target="_blank" rel="noopener"><Icon name="openIn" :size="18" />In neuem Tab</a>
				<span class="spacer"></span>
				<RowMenu :node="node" here :can-edit="editable" @share="dialog = { kind: 'share', node }" @rename="dialog = { kind: 'rename', node }" @move="dialog = { kind: 'move', node }" @remove="dialog = { kind: 'remove', node }" />
			</div>
		</template>

		<p v-if="error" class="error" role="alert">{{ error }}</p>
		<p v-if="notice" class="notice" role="status">
			<Icon name="check" :size="16" :stroke="2" />{{ notice }}
			<button v-if="undo" class="link" type="button" @click="restoreDeleted">Rückgängig</button>
		</p>
		<p v-if="root && !root.scanned_at" class="muted" role="status">
			Die Ablage wird zum ersten Mal eingelesen – Inhalte erscheinen nach und nach.
		</p>

		<section v-if="node?.kind === 'dir'" class="folder" :class="{ dragging }">
			<div class="toolbar">
				<button type="button" class="text sort" @click="view.sort = view.sort === 'name' ? 'mtime' : 'name'">
					<Icon name="sort" :size="15" :stroke="1.6" />{{ view.sort === 'name' ? 'Name' : 'Zuletzt geändert' }}
				</button>
				<span class="spacer"></span>
				<button v-if="root" type="button" class="text rescan" :disabled="busy" @click="rescan">
					<Icon name="sync" :size="15" :stroke="1.6" />{{ busy ? 'Bitte warten …' : 'Neu einlesen' }}
				</button>
				<NuxtLink v-if="root && node.path.length === 1 && canEdit(root.role)" :to="root.kind === 'space' ? `/trash?root=${root.id}` : '/trash'" class="icon trash-link" aria-label="Papierkorb"><Icon name="trash" :size="18" :stroke="1.6" /></NuxtLink>
				<NuxtLink :to="{ path: '/search', query: { folder: node.id } }" class="icon" aria-label="In diesem Ordner suchen"><Icon name="search" :size="18" :stroke="1.6" /></NuxtLink>
				<button type="button" class="icon" aria-label="Listenansicht" :aria-pressed="view.layout === 'list'" @click="view.layout = 'list'"><Icon name="list" :size="18" :stroke="1.6" /></button>
				<button type="button" class="icon" aria-label="Rasteransicht" :aria-pressed="view.layout === 'grid'" @click="view.layout = 'grid'"><Icon name="grid" :size="18" :stroke="1.6" /></button>
			</div>
			<ul v-if="sorted.length && view.layout === 'list'" class="rows">
				<li v-for="c in sorted" :key="c.id">
					<NuxtLink :to="`/files/${c.id}`" class="open" :aria-describedby="`meta-${c.id}`">
						<FileMark :node="c" />
						<span class="text-col">
							<span class="name">{{ c.name }}</span>
							<span :id="`meta-${c.id}`" class="sub" aria-hidden="true"><span v-if="c.data_class" class="tag klass"><Icon :name="c.data_class === 'local' ? 'lock' : 'cloud'" :size="10" :stroke="2" />{{ CLASS_LABEL[c.data_class] }}</span>{{ meta(c) }}</span>
						</span>
					</NuxtLink>
					<RowMenu :node="c" :can-edit="editable" @share="dialog = { kind: 'share', node: c }" @rename="dialog = { kind: 'rename', node: c }" @move="dialog = { kind: 'move', node: c }" @remove="dialog = { kind: 'remove', node: c }" @data-class="dialog = { kind: 'class', id: c.id }" />
				</li>
			</ul>
			<ul v-else-if="sorted.length" class="tiles">
				<li v-for="c in sorted" :key="c.id">
					<NuxtLink :to="`/files/${c.id}`" class="open">
						<span class="pic">
							<img v-if="thumbUrl(c, 256)" :src="thumbUrl(c, 256)!" alt="" loading="lazy" />
							<FileMark v-else :node="c" big />
						</span>
						<span class="name">{{ c.name }}</span>
					</NuxtLink>
					<span class="foot"><span class="sub"><span v-if="c.data_class" class="tag klass"><Icon :name="c.data_class === 'local' ? 'lock' : 'cloud'" :size="10" :stroke="2" />{{ CLASS_LABEL[c.data_class] }}</span>{{ meta(c) }}</span><RowMenu :node="c" :can-edit="editable" @share="dialog = { kind: 'share', node: c }" @rename="dialog = { kind: 'rename', node: c }" @move="dialog = { kind: 'move', node: c }" @remove="dialog = { kind: 'remove', node: c }" @data-class="dialog = { kind: 'class', id: c.id }" /></span>
				</li>
			</ul>
			<p v-else-if="!loading" class="muted empty">
				{{ editable ? 'Dieser Ordner ist leer. Dateien hierher ziehen oder „Hochladen“ wählen.' : 'Dieser Ordner ist leer.' }}
			</p>
			<p v-if="dragging" class="drop">Loslassen zum Hochladen</p>
		</section>

		<template v-else-if="node">
			<FilePreview :node="node" />
			<div class="tabs" role="tablist" aria-label="Details">
				<button role="tab" type="button" :aria-selected="tab === 'info'" @click="tab = 'info'">Info</button>
				<button role="tab" type="button" :aria-selected="tab === 'versionen'" @click="tab = 'versionen'">
					Versionen{{ versions.length ? ` (${versions.length})` : '' }}
				</button>
				<button role="tab" type="button" :aria-selected="tab === 'zugriff'" @click="tab = 'zugriff'">Zugriff</button>
				<button role="tab" type="button" :aria-selected="tab === 'aktivitaet'" @click="tab = 'aktivitaet'">Aktivität</button>
			</div>
			<section v-if="tab === 'info'" class="info" role="tabpanel" aria-label="Info">
				<div class="kv"><span>Speicherort</span><span>{{ place }}</span></div>
				<div class="kv"><span>Geändert</span><span>{{ formatDate(node.mtime) }}</span></div>
				<div class="kv"><span>Größe</span><span>{{ formatSize(node.size) }}</span></div>
				<div class="kv"><span>Typ</span><span>{{ node.mime ?? '–' }}</span></div>
				<div class="kv">
					<span>Cloud-Analyse</span>
					<span>{{ node.data_class.class === 'cloud' ? 'Erlaubt' : 'Nicht erlaubt' }} ({{ CLASS_LABEL[node.data_class.class] }}, {{ classSource(node.data_class) }})</span>
				</div>
			</section>
			<section v-else-if="tab === 'aktivitaet'" class="history" role="tabpanel" aria-label="Aktivität">
				<ActivityList v-if="history?.length" :groups="history" :show-folder="false" />
				<p v-else-if="history" class="muted">Noch nichts.</p>
			</section>
			<section v-else-if="tab === 'zugriff'" class="access" role="tabpanel" aria-label="Zugriff">
				<div class="kv"><span>Deine Rolle</span><span>{{ ROLE_LABEL[node.role] }}</span></div>
				<div v-if="access?.owner" class="kv"><span>Gehört</span><span>{{ access.owner }}</span></div>
				<div v-if="access?.space" class="kv"><span>Ablage</span><span>{{ access.space }}</span></div>
				<div v-for="s in access?.shares ?? []" :key="s.id" class="kv">
					<span>{{ s.to.name }}<template v-if="s.inherited"> (über „{{ s.node_name }}“)</template></span>
					<span>{{ s.expired ? 'abgelaufen' : ROLE_LABEL[s.role] }}</span>
				</div>
				<button class="new-version" type="button" @click="dialog = { kind: 'share', node }"><Icon name="shared" :size="18" />{{ access?.can_share ? 'Teilen' : 'Wer hat Zugriff?' }}</button>
			</section>
			<section v-else class="versions" role="tabpanel" aria-label="Versionen">
				<ul class="rows">
					<li class="current">
						<Icon name="history" :size="18" class="muted" />
						<span class="text-col"><span class="name">{{ formatDate(node.mtime) }}</span><span class="sub">{{ formatSize(node.size) }}</span></span>
						<span class="tag">Aktuell</span>
					</li>
					<li v-for="v in versions" :key="v.id" class="old">
						<Icon name="history" :size="18" class="muted" />
						<span class="text-col">
							<span class="name">{{ formatDate(v.mtime ?? v.created_at) }}</span>
							<span class="sub">{{ formatSize(v.size) }} · ersetzt von {{ v.created_by ?? 'außerhalb von xlrx' }}</span>
						</span>
						<a class="small-link" :href="`/api/versions/${v.id}/content`" download>Herunterladen</a>
						<button v-if="editable" class="link small-link" type="button" :disabled="busy" @click="restoreVersion(v)">Wiederherstellen</button>
					</li>
				</ul>
				<p v-if="!versions.length" class="muted small">
					Noch keine früheren Fassungen. Wird die Datei über xlrx ersetzt, bleibt die bisherige hier erhalten.
				</p>
				<button v-if="editable" class="new-version" :disabled="busy" @click="versionPicker?.click()"><Icon name="upload" :size="18" />Neue Fassung hochladen</button>
				<input ref="versionPicker" type="file" hidden data-testid="version-input" @change="newVersion" />
			</section>
		</template>

		<NameDialog v-if="dialog?.kind === 'folder'" title="Neuer Ordner" action="Anlegen" @submit="newFolder" @cancel="dialog = null" />
		<NameDialog
			v-else-if="dialog?.kind === 'rename'"
			title="Umbenennen"
			action="Fertig"
			:initial="dialog.node.name"
			@submit="(name) => dialog?.kind === 'rename' && rename(dialog.node, name)"
			@cancel="dialog = null"
		/>
		<MoveDialog
			v-else-if="dialog?.kind === 'move' && node"
			:node="dialog.node"
			:root-node="root?.node_id ?? node.path[0]!.id"
			@move="(p) => dialog?.kind === 'move' && move(dialog.node, p)"
			@cancel="dialog = null"
		/>
		<ShareDialog v-else-if="dialog?.kind === 'share'" :node="dialog.node" @close="dialog = null" @changed="load" />
		<DataClassDialog v-else-if="dialog?.kind === 'class'" :node-id="dialog.id" @close="dialog = null" @changed="load" />
		<ConfirmDialog
			v-else-if="dialog?.kind === 'remove'"
			title="In den Papierkorb verschieben?"
			:text="`„${dialog.node.name}“ verschwindet aus dem Ordner. 30 Tage lang lässt ${dialog.node.kind === 'dir' ? 'er' : 'sie'} sich wiederherstellen.`"
			action="In den Papierkorb"
			:node="dialog.node"
			danger
			@confirm="dialog?.kind === 'remove' && remove(dialog.node)"
			@cancel="dialog = null"
		/>
	</main>
</template>

<style scoped>
.crumbs { display: flex; flex-wrap: wrap; align-items: center; gap: 2px; font-size: 13px; color: var(--muted); margin: 0 0 6px -4px; }
.crumbs a { padding: 2px 4px; }
.title-row { display: flex; align-items: flex-end; gap: 16px; flex-wrap: wrap; padding-bottom: 16px; }
.titles { flex: 1; min-width: 12rem; }
.titles h1 { margin: 0 0 8px; }
.meta { margin: 0; font-size: 13px; color: var(--muted); }
.class-link { display: inline-flex; align-items: center; gap: 4px; min-height: 0; padding: 0; font-size: 13px; color: var(--ink-3); vertical-align: baseline; }
.klass { margin-right: 6px; vertical-align: 1px; }
.tools { display: flex; gap: 8px; }
.file-title { font-size: 23px; line-height: 1.2; letter-spacing: -0.02em; margin: 4px 0 6px; }
.file-actions { margin: 14px 0 18px; }
.notice { display: flex; align-items: center; gap: 8px; margin: 12px 0 0; font-size: 14px; }
.notice .link { margin-left: 4px; }
.folder { position: relative; border-top: 1px dashed var(--dash); }
.folder.dragging { outline: 2px dashed var(--accent); outline-offset: 6px; }
.drop { position: absolute; inset: 0; display: grid; place-items: center; margin: 0; background: color-mix(in srgb, var(--paper) 85%, transparent); color: var(--accent); font-weight: 500; }
.toolbar { display: flex; align-items: center; gap: 4px; padding: 4px 0; }
.toolbar .text { min-height: 40px; padding: 0 6px; gap: 6px; font-size: 13px; color: var(--ink-2); }
.toolbar .icon { width: 40px; height: 40px; color: var(--faint); }
.toolbar .icon[aria-pressed='true'] { color: var(--ink); }
.rows li { gap: 0; }
.open { flex: 1; min-width: 0; min-height: 58px; display: flex; align-items: center; gap: 14px; padding-left: 4px; text-decoration: none; }
.text-col { flex: 1; min-width: 0; display: flex; flex-direction: column; gap: 3px; }
.name { font-size: 15px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.open:hover .name { text-decoration: underline; text-decoration-color: var(--line-strong); }
.sub { font-size: 12px; color: var(--muted); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.tiles { list-style: none; margin: 8px 0 0; padding: 0; display: grid; grid-template-columns: repeat(auto-fill, minmax(150px, 1fr)); gap: 18px 14px; }
.tiles .open { flex-direction: column; align-items: stretch; gap: 8px; padding: 0; min-height: 0; }
.pic { height: 120px; border: 1px solid var(--dash); background: var(--paper-hi); display: flex; align-items: center; justify-content: center; overflow: hidden; }
.pic img { width: 100%; height: 100%; object-fit: cover; }
.foot { display: flex; align-items: center; justify-content: space-between; margin-top: -6px; }
.foot :deep(.dots) { width: 36px; height: 36px; margin-right: -8px; }
.empty { padding: 1.5rem 0; margin: 0; }
.tabs { margin-top: 20px; }
.info, .versions { padding-top: 4px; }
.versions .rows li { gap: 12px; min-height: 54px; }
.small-link { font-size: 13px; }
.versions .current .name { font-weight: 400; }
.new-version { margin-top: 16px; }
.small { font-size: 13px; }
@media (max-width: 47.99rem) {
	.desktop { display: none; }
	.toolbar .rescan { font-size: 12.5px; }
}
</style>
