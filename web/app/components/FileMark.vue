<script setup lang="ts">
// What stands for a file in lists (as in the design): a folder outline, a thumbnail of a picture,
// or a small sheet of paper with the file's extension.
const props = withDefaults(
	defineProps<{ node: Pick<NodeInfo, 'id' | 'kind' | 'name' | 'mime' | 'rev'>; big?: boolean }>(),
	{ big: false }
);
const failed = ref(false);
watch(
	() => props.node.rev,
	() => (failed.value = false)
);
const thumb = computed(() => (failed.value ? null : thumbUrl(props.node, props.big ? 256 : 64)));
const ext = computed(() => {
	const dot = props.node.name.lastIndexOf('.');
	return dot > 0 ? props.node.name.slice(dot + 1, dot + 5).toUpperCase() : '';
});
</script>

<template>
	<span class="mark" :class="{ big }" aria-hidden="true">
		<Icon v-if="node.kind === 'dir'" name="folder" :size="big ? 34 : 26" :stroke="1.3" />
		<img v-else-if="thumb" class="thumb" :src="thumb" alt="" loading="lazy" @error="failed = true" />
		<span v-else class="chip">{{ ext }}</span>
	</span>
</template>

<style scoped>
.mark { flex: none; width: 38px; height: 38px; display: inline-flex; align-items: center; justify-content: center; }
.thumb { width: 36px; height: 36px; object-fit: cover; border: 1px solid var(--line-strong); background: var(--paper-hi); }
.chip {
	width: 28px; height: 36px; background: var(--paper-hi); border: 1px solid color-mix(in srgb, var(--ink) 40%, transparent);
	border-radius: 2px; display: flex; align-items: flex-end; justify-content: center; padding-bottom: 4px;
	font-family: var(--mono); font-size: 8px; font-weight: 500; letter-spacing: 0.04em; color: var(--ink);
}
.big { width: 52px; height: 52px; }
.big .thumb { width: 52px; height: 52px; }
.big .chip { width: 38px; height: 50px; font-size: 10px; padding-bottom: 6px; }
</style>
