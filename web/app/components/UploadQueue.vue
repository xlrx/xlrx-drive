<script setup lang="ts">
// Progress of the running uploads (bottom right; on phones above the navigation).
defineProps<{ items: UploadItem[]; active: boolean }>();
const emit = defineEmits<{ close: []; resume: [key: number] }>();
const label: Record<UploadItem['state'], string> = {
	waiting: 'wartet',
	asking: 'wartet auf Antwort',
	uploading: 'lädt hoch',
	done: 'fertig',
	skipped: 'übersprungen',
	error: 'Fehler'
};
const pct = (i: UploadItem) => Math.round((100 * i.loaded) / Math.max(i.size, 1));
</script>

<template>
	<section v-if="items.length" class="queue" aria-label="Uploads">
		<header>
			<strong>{{ active ? 'Wird hochgeladen …' : 'Uploads abgeschlossen' }}</strong>
			<button v-if="!active" type="button" class="icon close" aria-label="Uploads schließen" @click="emit('close')">
				<Icon name="x" :size="14" :stroke="2" />
			</button>
		</header>
		<ul>
			<li v-for="i in items" :key="i.key" :class="i.state">
				<span class="line">
					<span class="name">{{ i.name }}</span>
					<span class="state mono">{{ i.state === 'uploading' ? `${pct(i)} %` : label[i.state] }}</span>
				</span>
				<span v-if="i.state === 'uploading'" class="bar" role="progressbar" :aria-valuenow="pct(i)" aria-valuemin="0" aria-valuemax="100" :aria-label="i.name"><span :style="{ width: `${pct(i)}%` }"></span></span>
				<span v-if="i.error" class="error">{{ i.error }}</span>
				<button v-if="i.state === 'error'" type="button" class="again" @click="emit('resume', i.key)">
					{{ i.resumable ? 'Fortsetzen' : 'Erneut versuchen' }}
				</button>
			</li>
		</ul>
	</section>
</template>

<style scoped>
.queue {
	position: fixed; right: 16px; bottom: 16px; width: min(22rem, calc(100% - 32px)); z-index: 30; max-height: 50vh; overflow: auto;
	background: var(--paper); border: 1px solid var(--line-strong); border-radius: 14px; box-shadow: var(--shadow-float); padding: 10px 16px 12px;
}
header { display: flex; align-items: center; justify-content: space-between; min-height: 32px; font-size: 14px; }
header strong { font-weight: 500; }
.close { width: 32px; height: 32px; margin-right: -8px; }
ul { list-style: none; margin: 4px 0 0; padding: 0; }
li { display: flex; flex-direction: column; gap: 6px; padding: 8px 0; border-top: 1px solid var(--line); }
.line { display: flex; justify-content: space-between; gap: 8px; align-items: baseline; }
.name { font-size: 14px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.state { flex: none; font-size: 11px; color: var(--ink-3); }
.bar { height: 2px; background: var(--line); }
.bar span { display: block; height: 2px; background: var(--ink); }
.done .state { color: var(--ok); }
.error .state { color: var(--danger); }
li .error { font-size: 12.5px; margin: 0; }
.again { align-self: flex-start; min-height: 32px; padding: 0 14px; font-size: 13px; border-radius: 16px; }
@media (max-width: 47.99rem) {
	.queue { left: 14px; right: 14px; width: auto; bottom: calc(92px + env(safe-area-inset-bottom)); }
}
</style>
