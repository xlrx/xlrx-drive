<script setup lang="ts">
// Actions of a file or folder (as in the design): the "⋯" button opens a sheet from the bottom on
// narrow screens, a menu beside the button on wide ones.
const props = withDefaults(defineProps<{ node: NodeInfo; here?: boolean; canEdit?: boolean }>(), {
	here: false,
	canEdit: true
});
const emit = defineEmits<{ rename: []; move: []; remove: []; share: []; dataClass: [] }>();
const open = ref(false);
const button = ref<HTMLButtonElement | null>(null);
const panel = ref<HTMLElement | null>(null);
/** Where the menu opens on wide screens: below the button, or above it when there is no room. */
const pos = ref<Record<string, string>>({});
const id = useId();

const isFile = computed(() => props.node.kind === 'file');
const meta = computed(() =>
	[isFile.value ? formatSize(props.node.size) : 'Ordner', formatShortDate(props.node.mtime)].filter(Boolean).join(' · ')
);

function show() {
	const r = button.value?.getBoundingClientRect();
	if (r) {
		const right = `${window.innerWidth - r.right}px`;
		const below = window.innerHeight - r.bottom;
		pos.value =
			below >= 440 || below >= r.top
				? { '--top': `${r.bottom + 6}px`, '--bottom': 'auto', '--right': right, '--room': `${below - 18}px` }
				: { '--top': 'auto', '--bottom': `${window.innerHeight - r.top + 6}px`, '--right': right, '--room': `${r.top - 18}px` };
	}
	open.value = true;
	nextTick(() => panel.value?.querySelector<HTMLElement>('[role="menuitem"]')?.focus());
}
function close() {
	open.value = false;
	button.value?.focus();
}
function pick(what: 'rename' | 'move' | 'remove' | 'share' | 'dataClass') {
	open.value = false;
	if (what === 'rename') emit('rename');
	else if (what === 'move') emit('move');
	else if (what === 'share') emit('share');
	else if (what === 'dataClass') emit('dataClass');
	else emit('remove');
}
function onKey(e: KeyboardEvent) {
	if (!open.value) return;
	if (e.key === 'Escape') close();
	if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
		const items = Array.from(panel.value?.querySelectorAll<HTMLElement>('[role="menuitem"]') ?? []);
		const i = items.indexOf(document.activeElement as HTMLElement);
		items[(i + (e.key === 'ArrowDown' ? 1 : items.length - 1)) % items.length]?.focus();
		e.preventDefault();
	}
}
onMounted(() => window.addEventListener('keydown', onKey));
onBeforeUnmount(() => window.removeEventListener('keydown', onKey));
</script>

<template>
	<button
		ref="button"
		class="icon dots"
		type="button"
		:aria-label="`Aktionen für ${node.name}`"
		aria-haspopup="menu"
		:aria-expanded="open"
		@click="open ? close() : show()"
	>
		<Icon name="more" :size="19" />
	</button>
	<Teleport to="body">
		<div v-if="open" class="scrim" role="presentation" @click.self="close">
			<div
				ref="panel"
				class="panel"
				role="menu"
				:aria-labelledby="id"
				:style="pos"
			>
				<span class="handle" aria-hidden="true"></span>
				<div class="head">
					<FileMark :node="node" />
					<span class="what">
						<span :id="id" class="name">{{ node.name }}</span>
						<span class="meta">{{ meta }}</span>
					</span>
					<button type="button" class="icon close" aria-label="Schließen" @click="close">
						<span class="ring"><Icon name="x" :size="13" :stroke="2" /></span>
					</button>
				</div>
				<div v-if="isFile" class="quick">
					<a role="menuitem" class="tile accent" :href="contentUrl(node.id)" download @click="open = false">
						<Icon name="download" :size="21" /><span>Herunterladen</span>
					</a>
					<a
						v-if="opensInBrowser(node.mime)"
						role="menuitem"
						class="tile"
						:href="contentUrl(node.id, true)"
						target="_blank"
						rel="noopener"
						@click="open = false"
					>
						<Icon name="openIn" :size="21" /><span>Neuer Tab</span>
					</a>
				</div>
				<hr v-if="isFile" />
				<NuxtLink v-if="!here" role="menuitem" class="item" :to="`/files/${node.id}`" @click="open = false">
					<Icon :name="isFile ? 'eye' : 'folder'" /><span>{{ isFile ? 'Vorschau' : 'Öffnen' }}</span>
				</NuxtLink>
				<button role="menuitem" type="button" class="item" @click="pick('share')">
					<Icon name="shared" /><span>Teilen</span>
				</button>
				<button v-if="!isFile" role="menuitem" type="button" class="item" @click="pick('dataClass')">
					<Icon name="lock" /><span>Datenklasse</span>
				</button>
				<button v-if="canEdit" role="menuitem" type="button" class="item" @click="pick('rename')">
					<Icon name="edit" /><span>Umbenennen</span>
				</button>
				<button v-if="canEdit" role="menuitem" type="button" class="item" @click="pick('move')">
					<Icon name="move" /><span>Verschieben</span>
				</button>
				<NuxtLink
					v-if="isFile && !here"
					role="menuitem"
					class="item"
					:to="`/files/${node.id}?tab=versionen`"
					@click="open = false"
				>
					<Icon name="history" /><span>Versionen</span>
				</NuxtLink>
				<button v-if="canEdit" role="menuitem" type="button" class="item danger" @click="pick('remove')">
					<Icon name="trash" /><span>In den Papierkorb</span>
				</button>
			</div>
		</div>
	</Teleport>
</template>

<style scoped>
.dots { color: var(--ink-3); }
.scrim { position: fixed; inset: 0; z-index: 40; }
.panel {
	position: fixed; top: var(--top); bottom: var(--bottom); right: var(--right); width: 19rem; max-height: var(--room); overflow: auto;
	background: var(--paper); border: 1px solid var(--line-strong); border-radius: 14px; box-shadow: var(--shadow-float); padding: 6px 0;
}
.handle { display: none; }
.head { display: flex; align-items: center; gap: 12px; padding: 8px 6px 10px 16px; }
.what { flex: 1; min-width: 0; display: flex; flex-direction: column; gap: 2px; }
.name { font-size: 15px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.meta { font-size: 12.5px; color: var(--muted); }
.ring { width: 30px; height: 30px; border-radius: 15px; border: 1px solid var(--line-strong); display: flex; align-items: center; justify-content: center; }
.quick { display: grid; grid-template-columns: repeat(4, minmax(0, 1fr)); gap: 8px; padding: 0 12px 12px; }
.tile {
	height: 68px; border-radius: 12px; border: 1px solid var(--line-strong); display: flex; flex-direction: column;
	align-items: center; justify-content: center; gap: 6px; font-size: 12px; text-decoration: none; text-align: center; color: var(--ink);
}
.tile.accent { background: var(--accent); border-color: var(--accent); color: var(--accent-text); }
.tile:hover:not(.accent) { background: var(--fill); }
.quick .tile { grid-column: span 2; }
.item {
	display: flex; align-items: center; justify-content: flex-start; gap: 16px; width: 100%; min-height: 47px; padding: 0 20px;
	border: 0; border-radius: 0; border-bottom: 1px solid var(--line); font-size: 15px; text-decoration: none; color: var(--ink); background: none;
}
.item:last-child { border-bottom: 0; }
.item:hover, .item:focus-visible { background: var(--fill); outline: none; }
.item.danger { color: var(--danger); }

@media (max-width: 40rem) {
	.scrim { background: var(--scrim); }
	.panel {
		top: auto; right: 0; left: 0; bottom: 0; width: auto; max-height: calc(100vh - 24px); border: 0;
		border-top: 1px solid var(--line-strong); border-radius: 18px 18px 0 0; box-shadow: var(--shadow-sheet);
		padding: 8px 0 calc(24px + env(safe-area-inset-bottom));
	}
	.handle { display: block; margin: 0 auto 4px; width: 34px; height: 4px; border-radius: 2px; background: var(--line-strong); }
	.head { padding: 14px 10px 14px 24px; }
	.quick { padding: 0 16px 14px; }
	.quick .tile { grid-column: span 1; }
	.item { padding: 0 24px; font-size: 15.5px; }
}
</style>
