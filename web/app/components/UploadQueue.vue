<script setup lang="ts">
// Progress of the running uploads (bottom right).
defineProps<{ items: UploadItem[]; active: boolean }>();
const emit = defineEmits<{ close: [] }>();
const label: Record<UploadItem['state'], string> = {
	waiting: 'wartet',
	uploading: 'lädt hoch',
	done: 'fertig',
	skipped: 'übersprungen',
	error: 'Fehler'
};
</script>

<template>
	<section v-if="items.length" class="queue card" aria-label="Uploads">
		<header class="row">
			<strong>{{ active ? 'Wird hochgeladen …' : 'Uploads abgeschlossen' }}</strong>
			<span class="spacer"></span>
			<button v-if="!active" type="button" class="close" aria-label="Uploads schließen" @click="emit('close')">✕</button>
		</header>
		<ul>
			<li v-for="i in items" :key="i.key" :class="i.state">
				<div class="row line">
					<span class="name">{{ i.name }}</span>
					<span class="muted state">{{ i.state === 'uploading' ? `${Math.round((100 * i.loaded) / Math.max(i.size, 1))} %` : label[i.state] }}</span>
				</div>
				<progress v-if="i.state === 'uploading'" :value="i.loaded" :max="Math.max(i.size, 1)"></progress>
				<p v-if="i.error" class="error">{{ i.error }}</p>
			</li>
		</ul>
	</section>
</template>

<style scoped>
.queue { position: fixed; right: 1rem; bottom: 1rem; width: min(22rem, calc(100% - 2rem)); padding: 0.8rem 1rem; z-index: 8; max-height: 50vh; overflow: auto; }
.spacer { flex: 1; }
.close { border: 0; background: none; padding: 0.2rem 0.4rem; }
ul { list-style: none; margin: 0.5rem 0 0; padding: 0; }
li + li { margin-top: 0.5rem; }
.line { flex-wrap: nowrap; }
.name { flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.state { white-space: nowrap; font-size: 0.85rem; }
progress { width: 100%; height: 0.4rem; }
.done .state { color: var(--ok); }
.error .state { color: var(--danger); }
li .error { margin: 0.2rem 0 0; font-size: 0.85rem; }
</style>
