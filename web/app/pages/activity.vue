<script setup lang="ts">
// "Aktivität" (PLAN 8.3): what happened to everything I may see, newest first, grouped; filter
// all / by others / sharing.
useHead({ title: 'Aktivität – xlrx drive' });
type Who = 'all' | 'others' | 'shares';
const who = ref<Who>('all');
const groups = ref<ActivityGroup[]>([]);
const next = ref<string | null>(null);
const loaded = ref(false);
const busy = ref(false);
const error = ref('');

async function load(more = false) {
	busy.value = true;
	error.value = '';
	try {
		const q = new URLSearchParams({ who: who.value });
		if (more && next.value) q.set('before', next.value);
		const page = await apiGet<ActivityPage>(`/activity?${q}`);
		groups.value = more ? [...groups.value, ...page.groups] : page.groups;
		next.value = page.next;
	} catch (e) {
		error.value = errorMessage(e);
	} finally {
		busy.value = false;
		loaded.value = true;
	}
}
watch(who, () => load(), { immediate: true });
useLive().onAnyChange(() => load());
const FILTERS: { id: Who; label: string }[] = [
	{ id: 'all', label: 'Alle' },
	{ id: 'others', label: 'Von anderen' },
	{ id: 'shares', label: 'Freigaben' }
];
</script>

<template>
	<main class="page">
		<h1>Aktivität</h1>
		<div class="tabs" role="tablist" aria-label="Filter">
			<button v-for="f in FILTERS" :key="f.id" role="tab" type="button" :aria-selected="who === f.id" @click="who = f.id">{{ f.label }}</button>
		</div>
		<p v-if="error" class="error" role="alert">{{ error }}</p>
		<ActivityList v-if="groups.length" :groups="groups" />
		<p v-else-if="loaded" class="muted empty">
			{{ who === 'shares' ? 'Noch nichts geteilt.' : 'Noch keine Aktivität. Was du und andere hochladen, ändern oder teilen, erscheint hier.' }}
		</p>
		<button v-if="next" type="button" class="more" :disabled="busy" @click="load(true)">Ältere anzeigen</button>
	</main>
</template>

<style scoped>
.tabs { margin: 8px 0 4px; }
.empty { padding: 1.5rem 0; }
.more { margin-top: 16px; }
</style>
