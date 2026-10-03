<script setup lang="ts">
// Trash of "My Drive": restore, or delete for good. Entries go after 30 days by themselves.
const root = ref<RootInfo | null>(null);
const items = ref<TrashItem[]>([]);
const loading = ref(true);
const busy = ref(false);
const error = ref('');
const notice = ref('');
const purging = ref<TrashItem | null>(null);
useHead({ title: 'Papierkorb – xlrx drive' });

async function load() {
	try {
		root.value = (await apiGet<RootInfo[]>('/roots'))[0] ?? null;
		items.value = root.value ? await apiGet<TrashItem[]>(`/roots/${root.value.id}/trash`) : [];
	} catch (e) {
		error.value = errorMessage(e);
	} finally {
		loading.value = false;
	}
}
onMounted(load);
useLive().onRootChange(() => root.value?.id, load);

async function act(fn: () => Promise<void>) {
	busy.value = true;
	error.value = '';
	notice.value = '';
	try {
		await fn();
	} catch (e) {
		error.value = errorMessage(e);
	} finally {
		busy.value = false;
		await load();
	}
}

const restore = (t: TrashItem) =>
	act(async () => {
		const n = await apiPost<NodeInfo>(`/trash/${t.id}/restore`);
		notice.value =
			n.name === t.name ? `„${t.name}“ ist wieder da.` : `„${t.name}“ ist wieder da, als „${n.name}“.`;
	});

const purge = (t: TrashItem) =>
	act(async () => {
		purging.value = null;
		await apiDelete(`/trash/${t.id}`);
		notice.value = `„${t.name}“ ist endgültig gelöscht.`;
	});

const daysLeft = (t: TrashItem) =>
	Math.max(0, Math.ceil((new Date(t.deleted_at).getTime() + 30 * 86_400_000 - Date.now()) / 86_400_000));
const mark = (t: TrashItem) => ({ id: t.id, kind: t.kind, name: t.name, mime: null, rev: 0 });
</script>

<template>
	<main class="page">
		<h1>Papierkorb</h1>
		<p class="meta">Gelöschtes bleibt 30 Tage hier und lässt sich bis dahin wiederherstellen.</p>
		<p v-if="error" class="error" role="alert">{{ error }}</p>
		<p v-if="notice" class="notice" role="status"><Icon name="check" :size="16" :stroke="2" />{{ notice }}</p>
		<hr />
		<ul v-if="items.length" class="rows">
			<li v-for="t in items" :key="t.id">
				<FileMark :node="mark(t)" />
				<span class="text-col">
					<span class="name">{{ t.name }}</span>
					<span class="sub">aus {{ t.from }} · gelöscht {{ formatShortDate(t.deleted_at) }} · noch {{ daysLeft(t) }} {{ daysLeft(t) === 1 ? 'Tag' : 'Tage' }}</span>
				</span>
				<span class="acts">
					<button type="button" class="link" :disabled="busy" @click="restore(t)">Wiederherstellen</button>
					<button type="button" class="link danger" :disabled="busy" @click="purging = t">Endgültig löschen</button>
				</span>
			</li>
		</ul>
		<p v-else-if="!loading" class="muted empty">Der Papierkorb ist leer.</p>
		<ConfirmDialog
			v-if="purging"
			title="Endgültig löschen?"
			:text="`„${purging.name}“ wird sofort gelöscht. Das lässt sich nicht rückgängig machen.`"
			action="Endgültig löschen"
			:node="mark(purging)"
			danger
			@confirm="purging && purge(purging)"
			@cancel="purging = null"
		/>
	</main>
</template>

<style scoped>
.meta { margin: -6px 0 18px; font-size: 13px; color: var(--muted); }
.notice { display: flex; align-items: center; gap: 8px; font-size: 14px; }
.rows { border-top: 0; }
.rows li { gap: 14px; padding: 6px 0; }
.text-col { flex: 1; min-width: 0; display: flex; flex-direction: column; gap: 3px; }
.name { font-size: 15px; overflow-wrap: anywhere; }
.sub { font-size: 12px; color: var(--muted); }
.acts { display: flex; gap: 14px; flex-wrap: wrap; justify-content: flex-end; font-size: 13px; }
.acts .link { font-size: 13px; min-height: 32px; }
.empty { padding: 1.5rem 0; margin: 0; }
@media (max-width: 40rem) {
	.rows li { flex-wrap: wrap; }
	.acts { width: 100%; justify-content: flex-start; padding-left: 52px; }
}
</style>
