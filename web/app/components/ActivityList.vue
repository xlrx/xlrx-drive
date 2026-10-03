<script setup lang="ts">
// Entries of the activity stream (PLAN 8.3): who did what, when and where, with the items –
// pictures as a strip of thumbnails, other items by name.
const props = withDefaults(defineProps<{ groups: ActivityGroup[]; showFolder?: boolean }>(), { showFolder: true });

const initial = (g: ActivityGroup) => (g.actor?.name ?? '?').trim().charAt(0).toUpperCase() || '?';
const icon = (g: ActivityGroup) => (g.via === 'link' ? 'link' : g.via === 'nas' ? 'sync' : null);
/** Pictures whose thumbnail failed (damaged files) are left out of the strip. */
const broken = ref(new Set<number>());
const pictures = (g: ActivityGroup) => g.items.filter((i) => !i.deleted && !broken.value.has(i.id) && thumbUrl(i, 64));
const place = (g: ActivityGroup) => (g.folder ?? '').split('/').join(' › ');
const linkable = (g: ActivityGroup) => g.count === 1 && g.items[0] && !g.items[0].deleted;
const groups = computed(() => props.groups.map((g) => ({ g, s: activitySentence(g), pics: pictures(g) })));
</script>

<template>
	<ul class="activity">
		<li v-for="({ g, s, pics }, i) in groups" :key="`${g.kind}-${g.at}-${i}`">
			<span class="who" :class="{ mine: g.mine }" aria-hidden="true">
				<Icon v-if="icon(g)" :name="icon(g)!" :size="15" :stroke="1.6" />
				<template v-else>{{ initial(g) }}</template>
			</span>
			<div class="body">
				<p class="line">
					{{ s.pre }}<NuxtLink v-if="linkable(g)" :to="`/files/${g.items[0]!.id}`">{{ s.obj }}</NuxtLink><template v-else>{{ s.obj }}</template>{{ s.post }}
				</p>
				<p class="sub">
					<time :datetime="g.at">{{ formatAgo(g.at) }}</time>
					<template v-if="showFolder && g.folder">
						·
						<NuxtLink v-if="g.folder_id" :to="`/files/${g.folder_id}`">{{ place(g) }}</NuxtLink>
						<span v-else>{{ place(g) }}</span>
					</template>
				</p>
				<div v-if="pics.length > 1 || (pics.length === 1 && g.count > 1)" class="strip">
					<NuxtLink v-for="p in pics.slice(0, 6)" :key="p.id" :to="`/files/${p.id}`" :aria-label="p.name">
						<img :src="thumbUrl(p, 64)!" alt="" loading="lazy" @error="broken = new Set(broken).add(p.id)" />
					</NuxtLink>
					<span v-if="g.count > Math.min(pics.length, 6)" class="more">+{{ g.count - Math.min(pics.length, 6) }}</span>
				</div>
				<p v-else-if="g.count > 1" class="names">
					<template v-for="(it, k) in g.items" :key="it.id">
						<template v-if="k">, </template>
						<span v-if="it.deleted">{{ it.name }}</span>
						<NuxtLink v-else :to="`/files/${it.id}`">{{ it.name }}</NuxtLink>
					</template>
					<template v-if="g.count > g.items.length"> und {{ g.count - g.items.length }} weitere</template>
				</p>
			</div>
		</li>
	</ul>
</template>

<style scoped>
.activity { list-style: none; margin: 0; padding: 0; }
.activity > li { display: flex; gap: 14px; padding: 12px 0; border-bottom: 1px solid var(--line); }
.who {
	flex: none; width: 32px; height: 32px; border-radius: 16px; border: 1px solid var(--line-strong); display: inline-flex;
	align-items: center; justify-content: center; font-size: 13px; color: var(--ink-3); margin-top: 1px;
}
.who.mine { background: var(--fill); }
.body { flex: 1; min-width: 0; }
.line { margin: 0; font-size: 14.5px; line-height: 1.45; overflow-wrap: anywhere; }
.line a { color: inherit; }
.sub { margin: 3px 0 0; font-size: 12.5px; color: var(--muted); }
.sub a { color: inherit; }
.strip { display: flex; gap: 6px; margin-top: 8px; align-items: center; }
.strip img { width: 44px; height: 44px; object-fit: cover; border: 1px solid var(--line-strong); display: block; }
.more { font-size: 12.5px; color: var(--muted); padding-left: 2px; }
.names { margin: 6px 0 0; font-size: 13px; color: var(--ink-3); overflow-wrap: anywhere; }
.names a { color: inherit; }
</style>
