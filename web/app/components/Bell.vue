<script setup lang="ts">
// The bell (PLAN 8.4): what was shared with me and what arrived through my file requests – live.
// Opening the list marks everything in it as read.
const live = useLive();
const open = ref(false);
const list = ref<BellList | null>(null);
const button = ref<HTMLButtonElement | null>(null);
const id = useId();

async function refresh() {
	try {
		list.value = await apiGet<BellList>('/notifications');
		live.unread.value = list.value.unread;
	} catch {
		// keep what is shown
	}
}
onMounted(refresh);

async function show() {
	open.value = true;
	await refresh();
	if (list.value?.unread) await apiPost('/notifications/read', {}).catch(() => {});
}
function close() {
	open.value = false;
	button.value?.focus();
	if (list.value) list.value.items = list.value.items.map((i) => ({ ...i, read: true }));
}
function onKey(e: KeyboardEvent) {
	if (open.value && e.key === 'Escape') close();
}
onMounted(() => window.addEventListener('keydown', onKey));
onBeforeUnmount(() => window.removeEventListener('keydown', onKey));
const label = computed(() => (live.unread.value ? `Benachrichtigungen, ${live.unread.value} neu` : 'Benachrichtigungen'));
</script>

<template>
	<button ref="button" type="button" class="icon bell" :aria-label="label" aria-haspopup="dialog" :aria-expanded="open" @click="open ? close() : show()">
		<Icon name="bell" :size="20" :stroke="1.6" />
		<span v-if="live.unread.value" class="badge" aria-hidden="true">{{ live.unread.value > 9 ? '9+' : live.unread.value }}</span>
	</button>
	<Teleport to="body">
		<div v-if="open" class="scrim" role="presentation" @click.self="close">
			<div class="panel" role="dialog" :aria-labelledby="id">
				<div class="head">
					<h2 :id="id">Benachrichtigungen</h2>
					<button type="button" class="icon" aria-label="Schließen" @click="close"><Icon name="x" :size="16" :stroke="1.8" /></button>
				</div>
				<ul v-if="list?.items.length" class="items">
					<li v-for="n in list.items" :key="n.id" :class="{ unread: !n.read }">
						<NuxtLink :to="`/files/${n.node.id}`" @click="close">
							<FileMark :node="n.node" />
							<span class="text">
								<span class="line">{{ bellText(n) }}</span>
								<span v-if="n.kind === 'link_upload' && (n.details.count ?? 0) > 1" class="names">{{ n.details.names?.join(', ') }}</span>
								<span class="sub">{{ formatAgo(n.at) }}</span>
							</span>
						</NuxtLink>
					</li>
				</ul>
				<p v-else class="muted empty">Nichts Neues. Hier erscheint, was jemand mit dir teilt oder über deine Links schickt.</p>
			</div>
		</div>
	</Teleport>
</template>

<style scoped>
.bell { position: relative; color: var(--ink-3); }
.badge {
	position: absolute; top: 4px; right: 3px; min-width: 17px; height: 17px; padding: 0 4px; border-radius: 9px;
	background: var(--highlight); color: var(--on-highlight); font-size: 10.5px; font-weight: 600; line-height: 17px; text-align: center;
}
.scrim { position: fixed; inset: 0; z-index: 40; }
.panel {
	position: fixed; top: 60px; right: 16px; width: 23rem; max-height: calc(100vh - 80px); overflow: auto;
	background: var(--paper); border: 1px solid var(--line-strong); border-radius: 14px; box-shadow: var(--shadow-float);
}
.head { display: flex; align-items: center; justify-content: space-between; padding: 10px 8px 6px 18px; }
.head h2 { margin: 0; font-size: 16px; }
.items { list-style: none; margin: 0; padding: 0 0 6px; }
.items a { display: flex; gap: 12px; align-items: flex-start; padding: 10px 18px; text-decoration: none; color: var(--ink); }
.items a:hover { background: var(--fill); }
.items li.unread a { background: color-mix(in srgb, var(--highlight) 14%, transparent); }
.text { flex: 1; min-width: 0; display: flex; flex-direction: column; gap: 2px; }
.line { font-size: 14px; line-height: 1.4; overflow-wrap: anywhere; }
.names { font-size: 12.5px; color: var(--ink-3); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.sub { font-size: 12px; color: var(--muted); }
.empty { padding: 8px 18px 18px; font-size: 14px; }
@media (max-width: 40rem) {
	.scrim { background: var(--scrim); }
	.panel { top: auto; left: 0; right: 0; bottom: 0; width: auto; border-radius: 18px 18px 0 0; padding-bottom: env(safe-area-inset-bottom); }
}
</style>
