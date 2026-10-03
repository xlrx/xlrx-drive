<script setup lang="ts">
// "Geteilt" (fourth section of the design's navigation): the shared roots I am a member of, and
// what others shared with me – newest first, with whose it is and what I may do.
useHead({ title: 'Geteilt – xlrx drive' });
const spaces = ref<RootInfo[]>([]);
const items = ref<SharedItem[] | null>(null);
const error = ref('');

async function load() {
	try {
		const [roots, shared] = await Promise.all([apiGet<RootInfo[]>('/roots'), apiGet<SharedItem[]>('/shared')]);
		spaces.value = roots.filter((r) => r.kind === 'space');
		items.value = shared;
	} catch (e) {
		error.value = errorMessage(e);
	}
}
onMounted(load);
useLive().onAnyChange(load);

const why = (i: SharedItem) =>
	[i.shared_by && i.shared_by !== i.owner ? `${i.owner} · geteilt von ${i.shared_by}` : i.owner, ROLE_LABEL[i.role], formatAgo(i.shared_at)]
		.filter(Boolean)
		.join(' · ');
</script>

<template>
	<main class="page">
		<h1>Geteilt</h1>
		<p v-if="error" class="error" role="alert">{{ error }}</p>

		<section v-if="spaces.length">
			<h2 class="label">Geteilte Ablagen</h2>
			<ul class="rows">
				<li v-for="s in spaces" :key="s.id">
					<NuxtLink :to="`/files/${s.node_id}`" class="open" :aria-describedby="`space-${s.id}`">
						<span class="mark" aria-hidden="true"><Icon name="users" :size="24" :stroke="1.3" /></span>
						<span class="text-col">
							<span class="name">{{ s.name }}</span>
							<span :id="`space-${s.id}`" class="sub" aria-hidden="true">{{ ROLE_LABEL[s.role] }}</span>
						</span>
					</NuxtLink>
				</li>
			</ul>
		</section>

		<section>
			<h2 class="label">Für mich freigegeben</h2>
			<ul v-if="items?.length" class="rows">
				<li v-for="i in items" :key="i.id">
					<NuxtLink :to="`/files/${i.id}`" class="open" :aria-describedby="`why-${i.id}`">
						<FileMark :node="i" />
						<span class="text-col">
							<span class="name">{{ i.name }}</span>
							<span :id="`why-${i.id}`" class="sub" aria-hidden="true">{{ why(i) }}</span>
						</span>
					</NuxtLink>
				</li>
			</ul>
			<p v-else-if="items" class="muted empty">
				Noch nichts. Was andere mit dir teilen, erscheint hier – Ordner und Dateien, mit dem, was du damit tun darfst.
			</p>
		</section>
	</main>
</template>

<style scoped>
section { margin-top: 18px; }
.label { margin: 0 0 6px; }
.rows .open { flex: 1; min-width: 0; display: flex; align-items: center; gap: 14px; min-height: 58px; text-decoration: none; color: inherit; }
.mark { flex: none; width: 38px; height: 38px; display: inline-flex; align-items: center; justify-content: center; }
.text-col { flex: 1; min-width: 0; display: flex; flex-direction: column; gap: 2px; }
.name { font-size: 15px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.sub { font-size: 12.5px; color: var(--muted); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.empty { padding: 12px 0; }
</style>
