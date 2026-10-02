<script setup lang="ts">
// The etched mountain landscape from the app design, by time of day. Fills the width of its box;
// ink and paper follow the page colors (`--scene-ink`, `--scene-paper`).
const props = defineProps<{ time?: TimeOfDay }>();

const box = ref<HTMLElement | null>(null);
const width = ref(0);
const still = ref(false);
const clock = useClock();
const zeit = computed(() => props.time ?? timeOfDay(clock.value));
const s = computed(() => (width.value ? landscape(zeit.value, width.value, still.value) : null));
const lamp = LAMP;
const tint = TINT;

let observer: ResizeObserver | undefined;
let motion: MediaQueryList | undefined;
const onMotion = () => (still.value = !!motion?.matches);

onMounted(() => {
	motion = window.matchMedia('(prefers-reduced-motion: reduce)');
	onMotion();
	motion.addEventListener('change', onMotion);
	const el = box.value!;
	width.value = sceneWidth(el.clientWidth);
	observer = new ResizeObserver(() => (width.value = sceneWidth(el.clientWidth)));
	observer.observe(el);
});
onBeforeUnmount(() => {
	observer?.disconnect();
	motion?.removeEventListener('change', onMotion);
});

/** Fades a group in once. */
const fade = (begin: string, dur: string) => ({
	attributeName: 'opacity',
	values: '0;1',
	keyTimes: '0;1',
	calcMode: 'spline',
	keySplines: '0.3 0.05 0.25 1',
	begin,
	dur,
	fill: 'freeze'
});

const SMOKE = 'M0 0c-1.6 -2 1.6 -4 0 -6s1.6 -4 0 -6';
const WINGS = 'M0 0q2.5 -2.6 5 0q2.5 -2.6 5 0;M0 0q2.5 1.4 5 0q2.5 1.4 5 0;M0 0q2.5 -2.6 5 0q2.5 -2.6 5 0';
const BIRDS = [
	{ at: 'translate(0 0) scale(0.9)', dur: '0.8s' },
	{ at: 'translate(11 -4) scale(0.7)', dur: '0.7s' },
	{ at: 'translate(20 2) scale(0.8)', dur: '0.9s' }
];
const WINDOWS = 'M17.2 -7.3H20.4V-3.6H17.2ZM24.2 -7.1H27.4V-3.4H24.2ZM5.6 -11.6H8.4V-9.2H5.6Z';
</script>

<template>
	<div ref="box" class="landscape">
		<svg
			v-if="s"
			role="img"
			:aria-label="s.label"
			:viewBox="`0 0 ${s.width} ${s.height}`"
			:width="s.width"
			:height="s.height"
			fill="none"
			stroke="currentColor"
		>
			<!-- Sky -->
			<g opacity="0">
				<animate v-bind="fade(s.t.b0, s.t.dur)" />
				<path :d="s.sky1" stroke-width="0.6" stroke-dasharray="0.6 2.4" :opacity="s.sky1o" />
				<path :d="s.rays" stroke-width="0.35" opacity="0.75" />
			</g>
			<g opacity="0">
				<animate v-bind="fade(s.t.b1, s.t.dur)" />
				<path :d="s.sky2" stroke-width="0.8" stroke-dasharray="0.8 1.2" opacity="0.8" />
				<circle :cx="s.ox" :cy="s.oy" :r="s.or" class="paper" stroke-width="0.7" />
				<path :d="s.moonH" stroke-width="0.45" opacity="0.8" />
			</g>
			<g opacity="0">
				<animate v-bind="fade(s.t.stars, s.t.slow)" />
				<path :d="s.stars" stroke-width="1.3" stroke-linecap="round" opacity="0.7" />
			</g>

			<!-- Mountains -->
			<path :d="s.a0" class="paper" stroke="none" />
			<g opacity="0">
				<animate v-bind="fade(s.t.b1, s.t.dur)" />
				<path :d="s.s0" stroke-width="0.50" :opacity="s.sh0O" stroke-linecap="round" />
			</g>
			<g opacity="0">
				<animate v-bind="fade(s.t.b2, s.t.dur)" />
				<path :d="s.o0" stroke-width="0.70" stroke-linejoin="round" />
			</g>
			<path :d="s.a1" class="paper" stroke="none" />
			<g opacity="0">
				<animate v-bind="fade(s.t.b2, s.t.dur)" />
				<path :d="s.s1" stroke-width="0.80" :opacity="s.sh1O" stroke-linecap="round" />
			</g>
			<g opacity="0">
				<animate v-bind="fade(s.t.b3, s.t.dur)" />
				<path :d="s.o1" stroke-width="1.10" stroke-linejoin="round" />
			</g>

			<!-- Meadow -->
			<path :d="s.mA" class="paper" stroke="none" />
			<g opacity="0">
				<animate v-bind="fade(s.t.b3, s.t.dur)" />
				<path :d="s.mT" stroke-width="0.55" :opacity="s.mTO" />
				<path :d="s.grass" stroke-width="0.45" stroke-linecap="round" />
				<path :d="s.mO" stroke-width="0.8" stroke-linejoin="round" />
			</g>

			<!-- House -->
			<g :transform="`translate(${s.hx} ${s.hy})`">
				<g opacity="0">
					<animate attributeName="opacity" values="0;0.8;0.35;1" keyTimes="0;0.25;0.4;1" :begin="s.t.light" :dur="s.t.lightDur" fill="freeze" />
					<circle cx="22" cy="-5" r="17" :fill="lamp" stroke="none" :opacity="s.glow">
						<animate attributeName="opacity" :values="s.glowV" dur="3.2s" repeatCount="indefinite" />
					</circle>
					<circle cx="22" cy="-5" r="8" :fill="lamp" stroke="none" :opacity="s.glow" />
					<ellipse cx="22" cy="2.4" rx="13" ry="2" :fill="lamp" stroke="none" :opacity="s.spill" />
				</g>
				<path d="M0 0V-10L7 -16.8L14 -10V0ZM14 0V-10L30.6 -9V0.5Z" class="paper" stroke="none" />
				<path d="M6.4 -17.6L24.6 -16.7L32.2 -9.1L14.4 -10.7Z" class="paper" stroke="none" />
				<g opacity="0">
					<animate v-bind="fade(s.t.b2, s.t.dur)" />
					<path :d="s.hL" stroke-width="0.28" />
					<path :d="s.hM" stroke-width="0.45" />
					<path
						d="M0 0V-10L7 -16.8L14 -10V0ZM14 -10L30.6 -9V0.5M-0.9 -9.3L7 -17.6L14.9 -10.4M6.4 -17.6L24.6 -16.7L32.2 -9.1L14.4 -10.7"
						stroke-width="0.6"
						stroke-linejoin="round"
					/>
				</g>
				<g opacity="0">
					<animate v-bind="fade(s.t.b3, s.t.dur)" />
					<path :d="s.hD" stroke-width="0.62" />
				</g>
				<g opacity="0">
					<animate v-bind="fade(s.t.b4, s.t.dur)" />
					<path d="M20.6 -15.2V-20.2H23.2V-15.8Z" fill="currentColor" stroke="none" />
					<path d="M20.4 -20.4H23.4" stroke-width="0.7" />
					<path d="M4.8 0V-6.8H8.4V0Z" fill="currentColor" stroke="none" />
					<path :d="WINDOWS" fill="currentColor" stroke-width="0.5" />
				</g>
				<g opacity="0">
					<animate attributeName="opacity" values="0;1;0.4;1" keyTimes="0;0.25;0.4;1" :begin="s.t.light" :dur="s.t.lightDur" fill="freeze" />
					<path :d="WINDOWS" :fill="s.win ?? 'currentColor'" stroke-width="0.5" />
				</g>
				<path d="M18.8 -7.3V-3.6M17.2 -5.45H20.4M25.8 -7.1V-3.4M24.2 -5.25H27.4" stroke-width="0.35" opacity="0">
					<animate attributeName="opacity" values="0;1" :begin="s.t.b4" :dur="s.t.dur" fill="freeze" />
				</path>
			</g>

			<!-- Pines -->
			<g opacity="0">
				<animate v-bind="fade(s.t.b3, s.t.dur)" />
				<path :d="s.pB" stroke-width="0.7" stroke-linecap="round" />
			</g>
			<g opacity="0">
				<animate v-bind="fade(s.t.b4, s.t.dur)" />
				<path :d="s.pF" fill="currentColor" stroke="none" />
				<path :d="s.pHi" class="paper-line" stroke-width="0.4" opacity="0.8" stroke-linecap="round" />
				<path :d="s.pBark" class="paper-line" stroke-width="0.3" opacity="0.6" />
			</g>

			<!-- Deer -->
			<g opacity="0">
				<animate v-bind="fade(s.t.deer, s.t.slow)" />
				<g v-if="s.d1" :transform="s.d1t">
					<path :d="s.deerG" fill="currentColor" stroke="none" />
					<path :d="s.legs" stroke-width="1.1" />
					<path :d="s.deerHi" class="paper-line" stroke-width="0.5" opacity="0.7" />
				</g>
				<g v-if="s.d2" :transform="s.d2t">
					<path :d="s.deerG" fill="currentColor" stroke="none">
						<animate attributeName="d" :values="s.swap" keyTimes="0;0.55;0.75;1" dur="11s" calcMode="discrete" repeatCount="indefinite" />
					</path>
					<path :d="s.legs" stroke-width="1.1" />
					<path :d="s.deerHi" class="paper-line" stroke-width="0.5" opacity="0.7" />
				</g>
			</g>

			<!-- Chimney smoke -->
			<g v-if="s.amb" :transform="`translate(${s.chx} ${s.chy})`">
				<path
					v-for="begin in [s.t.s1, s.t.s2, s.t.s3, s.t.s4]"
					:key="begin"
					:d="SMOKE"
					stroke-width="0.6"
					stroke-linecap="round"
					stroke-dasharray="0.8 0.9"
					opacity="0"
				>
					<animateTransform attributeName="transform" type="translate" values="0 0;4 -10;11 -24" dur="5.2s" :begin="begin" repeatCount="indefinite" />
					<animateTransform attributeName="transform" type="scale" values="0.7;1.3;2.1" dur="5.2s" :begin="begin" repeatCount="indefinite" additive="sum" />
					<animate attributeName="opacity" values="0;0.75;0.4;0" keyTimes="0;0.15;0.6;1" dur="5.2s" :begin="begin" repeatCount="indefinite" />
				</path>
			</g>

			<!-- Birds -->
			<g v-if="s.birds" :transform="`translate(-40 ${s.by})`">
				<animateTransform attributeName="transform" type="translate" :values="s.bv" :dur="s.bDur" :begin="s.t.amb" repeatCount="indefinite" />
				<path
					v-for="b in BIRDS"
					:key="b.at"
					d="M0 0q2.5 -2.6 5 0q2.5 -2.6 5 0"
					:transform="b.at"
					stroke-width="0.7"
					stroke-linecap="round"
				>
					<animate attributeName="d" :values="WINGS" :dur="b.dur" repeatCount="indefinite" />
				</path>
			</g>

			<rect :width="s.width" :height="s.height" :fill="tint" stroke="none" opacity="0" class="tint">
				<animate attributeName="opacity" :values="`0;${s.tint}`" :begin="s.t.b3" :dur="s.t.slow" fill="freeze" />
			</rect>
		</svg>
	</div>
</template>

<style scoped>
.landscape { color: var(--scene-ink, var(--text)); }
svg { display: block; width: 100%; height: auto; }
.paper { fill: var(--scene-paper, var(--bg)); }
.paper-line { stroke: var(--scene-paper, var(--bg)); }
.tint { mix-blend-mode: multiply; }
@media (prefers-color-scheme: dark) {
	/* Multiplying a warm tint onto a dark page would only muddy it. */
	.tint { display: none; }
}
</style>
