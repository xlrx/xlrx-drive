<script setup lang="ts">
// Preview of a single file. Content comes from the API with its own strict CSP; text is only
// ever shown as text, never interpreted.
const props = defineProps<{ node: NodeDetail }>();

/** Text previews read at most this much (range request). */
const TEXT_LIMIT = 256 * 1024;

const kind = computed(() => previewKind(props.node.mime));
const text = ref<string | null>(null);
const truncated = ref(false);
const failed = ref(false);

watch(
	() => [props.node.id, props.node.rev],
	async () => {
		text.value = null;
		truncated.value = false;
		failed.value = false;
		if (kind.value !== 'text') return;
		try {
			const res = await fetch(contentUrl(props.node.id), {
				credentials: 'same-origin',
				headers: { range: `bytes=0-${TEXT_LIMIT - 1}` }
			});
			if (!res.ok) throw new Error(String(res.status));
			const buf = await res.arrayBuffer();
			truncated.value = (props.node.size ?? 0) > buf.byteLength;
			text.value = new TextDecoder('utf-8').decode(buf);
		} catch {
			failed.value = true;
		}
	},
	{ immediate: true }
);

const message = computed(() => {
	if (failed.value) return 'Keine Vorschau möglich.';
	if (kind.value === 'text') return 'Wird geladen …';
	if (kind.value === 'office') return 'Die Vorschau für Office-Dokumente folgt.';
	return 'Keine Vorschau für diesen Dateityp.';
});
</script>

<template>
	<div class="preview card">
		<img
			v-if="kind === 'image' && !failed"
			:src="contentUrl(node.id, true)"
			:alt="node.name"
			@error="failed = true"
		/>
		<video
			v-else-if="kind === 'video' && !failed"
			:src="contentUrl(node.id, true)"
			controls
			preload="metadata"
			@error="failed = true"
		></video>
		<audio
			v-else-if="kind === 'audio' && !failed"
			:src="contentUrl(node.id, true)"
			controls
			preload="metadata"
			@error="failed = true"
		></audio>
		<iframe v-else-if="kind === 'pdf'" :src="contentUrl(node.id, true)" :title="node.name"></iframe>
		<div v-else-if="kind === 'text' && text !== null" class="text">
			<pre>{{ text }}</pre>
			<p v-if="truncated" class="muted">Gekürzt – die ganze Datei gibt es per Download.</p>
		</div>
		<div v-else class="none">
			<FileIcon :kind="iconKind(node)" />
			<p class="muted">{{ message }}</p>
		</div>
	</div>
</template>

<style scoped>
.preview { padding: 0; overflow: hidden; display: flex; justify-content: center; }
img, video { display: block; max-width: 100%; max-height: 75vh; object-fit: contain; }
audio { width: 100%; margin: 2rem; }
iframe { width: 100%; height: 80vh; border: 0; }
.text { width: 100%; }
pre {
	margin: 0;
	padding: 1rem 1.25rem;
	overflow: auto;
	max-height: 75vh;
	font: 0.85rem/1.5 ui-monospace, 'SF Mono', Menlo, monospace;
	white-space: pre-wrap;
	overflow-wrap: anywhere;
}
.text p { margin: 0; padding: 0.5rem 1.25rem 1rem; }
.none { padding: 3rem 1rem; text-align: center; }
.none :deep(svg) { width: 48px; height: 48px; }
</style>
