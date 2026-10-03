<script setup lang="ts">
// Preview of a single file. Content comes from the API with its own strict CSP; text is only
// ever shown as text, never interpreted. `base`: the API prefix (a public link has its own).
const props = withDefaults(defineProps<{ node: NodeInfo; base?: string }>(), { base: '/api' });

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
			const res = await fetch(contentUrl(props.node.id, opensInBrowser(props.node.mime), props.base), {
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
	<div class="preview" :class="kind">
		<img
			v-if="kind === 'image' && !failed"
			:src="contentUrl(node.id, true, base)"
			:alt="node.name"
			@error="failed = true"
		/>
		<video
			v-else-if="kind === 'video' && !failed"
			:src="contentUrl(node.id, true, base)"
			controls
			preload="metadata"
			@error="failed = true"
		></video>
		<audio
			v-else-if="kind === 'audio' && !failed"
			:src="contentUrl(node.id, true, base)"
			controls
			preload="metadata"
			@error="failed = true"
		></audio>
		<iframe v-else-if="kind === 'pdf'" :src="contentUrl(node.id, true, base)" :title="node.name"></iframe>
		<div v-else-if="kind === 'text' && text !== null" class="text">
			<pre>{{ text }}</pre>
			<p v-if="truncated" class="muted">Gekürzt – die ganze Datei gibt es per Download.</p>
		</div>
		<div v-else class="none">
			<FileMark :node="node" big />
			<p class="muted">{{ message }}</p>
		</div>
	</div>
</template>

<style scoped>
/* The design shows a document as a sheet of paper on the desk. */
.preview { display: flex; justify-content: center; border: 1px solid var(--dash); background: var(--paper-hi); overflow: hidden; }
.preview.pdf, .preview.text { background: var(--sheet); border-color: var(--line-strong); box-shadow: 0 2px 10px rgb(27 26 23 / 8%); color: #1b1a17; }
img, video { display: block; max-width: 100%; max-height: 75vh; object-fit: contain; }
audio { width: 100%; margin: 2rem; }
iframe { width: 100%; height: 80vh; border: 0; }
.text { width: 100%; }
pre {
	margin: 0;
	padding: 1.5rem 1.75rem;
	overflow: auto;
	max-height: 75vh;
	font: 13px/1.6 var(--mono);
	white-space: pre-wrap;
	overflow-wrap: anywhere;
}
.text p { margin: 0; padding: 0.5rem 1.75rem 1rem; color: #6f6a61; }
.none { padding: 3rem 1rem; display: flex; flex-direction: column; align-items: center; gap: 4px; text-align: center; }
</style>
