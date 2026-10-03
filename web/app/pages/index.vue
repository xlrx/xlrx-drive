<script setup lang="ts">
// Start (as in the design): date and greeting, the files changed last, hints – and the landscape.
type Recent = NodeInfo & { folder: string };
const { me } = useSession();
const clock = useClock();
const today = computed(() => clock.value.toLocaleDateString('de-DE', { weekday: 'long', day: 'numeric', month: 'long' }));
const recent = ref<Recent[] | null>(null);
const landscape = ref(true);
const uploads = useUploads();

async function load() {
	try {
		recent.value = await apiGet<Recent[]>('/recent?limit=8');
	} catch {
		recent.value = [];
	}
}
onMounted(() => {
	landscape.value = readAppearance().landscape;
	load();
});
watch(() => uploads.finished.value, load);
useLive().onAnyChange(load);

const ext = (name: string) => {
	const dot = name.lastIndexOf('.');
	return dot > 0 ? name.slice(dot + 1, dot + 5).toUpperCase() : '';
};
</script>

<template>
	<main class="page" :class="{ 'with-scenery': landscape }">
		<p class="date">{{ today }}</p>
		<h1 class="greeting">{{ greeting(timeOfDay(clock), me?.display_name ?? '') }}</h1>

		<hr />
		<section class="recent">
			<div class="title">
				<h2>Zuletzt geändert</h2>
				<NuxtLink to="/files">Alle Dateien</NuxtLink>
			</div>
			<ul v-if="recent?.length" class="cards">
				<li v-for="r in recent" :key="r.id">
					<NuxtLink :to="`/files/${r.id}`" class="card-link" :aria-describedby="`why-${r.id}`">
						<span class="preview">
							<img v-if="thumbUrl(r, 256)" :src="thumbUrl(r, 256)!" alt="" loading="lazy" />
							<span v-else class="doc" aria-hidden="true"><span></span><span></span><span></span><span></span><span></span></span>
							<span v-if="ext(r.name)" class="tag ext" aria-hidden="true">{{ ext(r.name) }}</span>
						</span>
						<span class="name">{{ r.name }}</span>
						<span :id="`why-${r.id}`" class="why" aria-hidden="true">{{ formatAgo(r.mtime) }} · {{ r.folder }}</span>
					</NuxtLink>
				</li>
			</ul>
			<p v-else-if="recent" class="muted empty">
				Noch keine Dateien. Mit <strong>Neu</strong> lädst du die ersten hoch – oder lege sie in den Ordner
				„Drive“ auf dem NAS.
			</p>
		</section>

		<template v-if="me && (me.recovery_codes_left < 4 || !me.passkeys.length)">
			<hr />
			<section class="hints">
				<h2>Hinweise</h2>
				<div v-if="me.recovery_codes_left < 4" class="note" role="alert">
					Nur noch {{ me.recovery_codes_left }} Wiederherstellungscodes übrig.
					<p><NuxtLink to="/settings/security">Neue erzeugen</NuxtLink>, damit du nie ausgesperrt bist.</p>
				</div>
				<div v-if="!me.passkeys.length" class="note">
					Mit einem Passkey anmelden
					<p>
						Per Face ID oder Touch ID – ohne Passwort und sicher gegen Phishing.
						<NuxtLink to="/settings/security">Passkey hinzufügen</NuxtLink>
					</p>
				</div>
			</section>
		</template>
		<Landscape v-if="landscape" class="scenery" />
	</main>
</template>

<style scoped>
.date { margin: 8px 0 6px; font-size: 13px; color: var(--muted); }
.greeting { font-size: 31px; line-height: 1.12; letter-spacing: -0.025em; margin: 0 0 22px; }
.recent, .hints { padding: 16px 0 18px; }
.title { display: flex; justify-content: space-between; align-items: baseline; }
.title h2 { margin: 0; }
.title a { font-size: 13px; }
.cards { list-style: none; margin: 12px 0 0; padding: 0; display: grid; grid-template-columns: repeat(auto-fill, minmax(146px, 1fr)); gap: 18px 14px; }
.card-link { display: flex; flex-direction: column; gap: 9px; text-decoration: none; }
.preview {
	position: relative; height: 98px; border: 1px solid var(--dash); background: var(--paper-hi); overflow: hidden;
	display: flex; align-items: flex-end; justify-content: center;
}
.preview img { position: absolute; inset: 0; width: 100%; height: 100%; object-fit: cover; }
.doc { width: 74px; height: 80px; border: 1px solid var(--line-strong); border-bottom: 0; background: var(--sheet); padding: 9px 8px; display: flex; flex-direction: column; gap: 5px; }
.doc span { height: 2px; background: #dad5cc; }
.doc span:first-child { width: 60%; height: 4px; background: #b9b3a8; }
.doc span:nth-child(4) { width: 80%; }
.ext { position: absolute; left: 6px; top: 6px; background: var(--paper); }
.name { font-size: 14px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.card-link:hover .name { text-decoration: underline; }
.why { font-size: 12px; line-height: 1.35; color: var(--muted); margin-top: -6px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.empty { font-size: 14px; }
.hints h2 { margin: 0 0 12px; }
.note + .note { margin-top: 10px; }
@media (max-width: 47.99rem) {
	.cards { grid-auto-flow: column; grid-template-columns: none; grid-auto-columns: 146px; overflow-x: auto; margin-inline: -24px; padding: 0 24px 4px; scroll-snap-type: x mandatory; scroll-padding-inline: 24px; }
	.cards li { scroll-snap-align: start; }
}
</style>
