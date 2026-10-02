<script setup lang="ts">
// Trash of "My Drive": restore, or delete for good. Entries go after 30 days by themselves.
const root = ref<RootInfo | null>(null);
const items = ref<TrashItem[]>([]);
const loading = ref(true);
const busy = ref(false);
const error = ref('');
const notice = ref('');
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
		if (!confirm(`„${t.name}“ endgültig löschen? Das lässt sich nicht rückgängig machen.`)) return;
		await apiDelete(`/trash/${t.id}`);
		notice.value = `„${t.name}“ ist endgültig gelöscht.`;
	});
</script>

<template>
	<main class="page">
		<h1>Papierkorb</h1>
		<p class="muted">Gelöschtes bleibt 30 Tage hier und lässt sich bis dahin wiederherstellen.</p>
		<p v-if="error" class="error" role="alert">{{ error }}</p>
		<p v-if="notice" role="status">{{ notice }}</p>
		<div class="card list">
			<table v-if="items.length">
				<thead>
					<tr>
						<th>Name</th>
						<th class="from">Gelöscht aus</th>
						<th class="when">Gelöscht am</th>
						<th class="act"><span class="sr-only">Aktionen</span></th>
					</tr>
				</thead>
				<tbody>
					<tr v-for="t in items" :key="t.id">
						<td>
							<span class="name"><FileIcon :kind="iconKind({ kind: t.kind, mime: null })" />{{ t.name }}</span>
						</td>
						<td class="from muted">{{ t.from }}</td>
						<td class="when muted">{{ formatShortDate(t.deleted_at) }}</td>
						<td class="act">
							<button type="button" :disabled="busy" @click="restore(t)">Wiederherstellen</button>
							<button type="button" class="danger" :disabled="busy" @click="purge(t)">
								Endgültig löschen
							</button>
						</td>
					</tr>
				</tbody>
			</table>
			<p v-else-if="!loading" class="muted empty">Der Papierkorb ist leer.</p>
		</div>
	</main>
</template>

<style scoped>
.list { padding: 0.25rem 0; margin-top: 1rem; }
.list table td, .list table th { padding-left: 1rem; padding-right: 1rem; vertical-align: middle; }
.list tbody tr:last-child td { border-bottom: 0; }
.name { display: flex; align-items: center; gap: 0.6rem; overflow-wrap: anywhere; }
.when { white-space: nowrap; }
.act { text-align: right; white-space: nowrap; }
.act button { padding: 0.35rem 0.7rem; font-size: 0.9rem; margin-left: 0.3rem; }
.empty { padding: 1.5rem 1rem; margin: 0; }
@media (max-width: 40rem) {
	.from, .when { display: none; }
	.act { white-space: normal; }
	.act button { margin: 0.15rem 0; }
}
</style>
