<script setup lang="ts">
// Small line icons for folders and file types (no icon font, nothing loaded from elsewhere).
const props = defineProps<{ kind: string }>();

const paths: Record<string, string> = {
	folder: 'M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z',
	file: 'M6 3h8l4 4v14H6zM14 3v4h4',
	doc: 'M6 3h8l4 4v14H6zM14 3v4h4M9 12h6M9 15h6M9 18h4',
	pdf: 'M6 3h8l4 4v14H6zM14 3v4h4M9 17v-4h1.5a1.5 1.5 0 0 1 0 3H9',
	office: 'M6 3h8l4 4v14H6zM14 3v4h4M9 12l1.5 6 1.5-4 1.5 4 1.5-6',
	image: 'M4 5h16v14H4zM4 16l5-5 4 4 3-3 4 4M15 9.5a1 1 0 1 0 0-.01',
	video: 'M4 6h12v12H4zM16 10l4-2v8l-4-2',
	audio: 'M9 18V6l10-2v12M9 18a2 2 0 1 1-4 0 2 2 0 0 1 4 0zM19 16a2 2 0 1 1-4 0 2 2 0 0 1 4 0z'
};
const d = computed(() => paths[props.kind] ?? paths.file);
</script>

<template>
	<svg
		class="icon"
		:class="kind"
		viewBox="0 0 24 24"
		width="20"
		height="20"
		fill="none"
		stroke="currentColor"
		stroke-width="1.6"
		stroke-linecap="round"
		stroke-linejoin="round"
		aria-hidden="true"
	>
		<path :d="d" />
	</svg>
</template>

<style scoped>
.icon { flex: none; color: var(--muted); }
.folder { color: var(--accent); fill: color-mix(in srgb, var(--accent) 15%, transparent); }
.pdf { color: #d14343; }
.office { color: #2f6fd6; }
.image, .video { color: #2e9d6a; }
.audio { color: #a35bd6; }
</style>
