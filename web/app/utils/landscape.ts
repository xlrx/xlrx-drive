// The hand-etched mountain landscape from the app design (canvas "Berglandschaft"), by time of day.
//
// Deterministic: the same time of day and width always give the same picture. At the design width
// of 390 the output matches the design exactly; wider scenes keep the density of peaks, stars and
// trees instead of stretching them. The field names follow the design source, so changes there can
// be carried over line by line.

export type TimeOfDay = 'Morgen' | 'Tag' | 'Abend' | 'Nacht';

/** Width and height of the scene in the design. */
export const SCENE_WIDTH = 390;
export const SCENE_HEIGHT = 170;

/** Window light and the warm evening tint. */
export const LAMP = '#F2C14E';
export const TINT = '#EFC547';

export function timeOfDay(d: Date = new Date()): TimeOfDay {
	const hr = d.getHours();
	return hr < 5 || hr >= 21 ? 'Nacht' : hr < 10 ? 'Morgen' : hr < 17 ? 'Tag' : 'Abend';
}

export function greeting(t: TimeOfDay, name: string): string {
	return {
		Morgen: `Guten Morgen, ${name}`,
		Tag: `Guten Tag, ${name}`,
		Abend: `Guten Abend, ${name}`,
		Nacht: `Noch wach, ${name}?`
	}[t];
}

interface Mood {
	sky: number;
	skyO: number;
	skyK: number;
	lit: number;
	len: number;
	shO: [number, number];
	/** Sun or moon: x and y as a fraction of the scene, radius. */
	orb: [number, number, number];
	moon: boolean;
	rays: '' | 'up' | 'all';
	stars: number;
	deer: number;
	birds: boolean;
	glow: number;
	tint: number;
	/** Hatching density of the house: front, side, roof. */
	hs: [number, number, number];
	ground: number;
	label: string;
}

const MOODS: Record<TimeOfDay, Mood> = {
	Morgen: { sky: 0.55, skyO: 0.42, skyK: 0.75, lit: 0.25, len: 0.9, shO: [0.5, 0.85], orb: [0.18, 0.13, 8], moon: false, rays: 'up', stars: 0, deer: 2, birds: true, glow: 0.12, tint: 0.04, hs: [0.25, 0.6, 0.45], ground: 0.45, label: 'am Morgen' },
	Tag: { sky: 0.35, skyO: 0.34, skyK: 0.5, lit: 0.2, len: 0.85, shO: [0.45, 0.8], orb: [0.82, 0.09, 6], moon: false, rays: 'all', stars: 0, deer: 1, birds: true, glow: 0, tint: 0, hs: [0.2, 0.55, 0.42], ground: 0.4, label: 'am Tag' },
	Abend: { sky: 0.8, skyO: 0.5, skyK: 1.1, lit: 0.42, len: 1.15, shO: [0.6, 0.95], orb: [0.53, 0.25, 11], moon: false, rays: 'up', stars: 0, deer: 2, birds: true, glow: 0.32, tint: 0.07, hs: [0.64, 0.8, 0.72], ground: 0.6, label: 'am Abend' },
	Nacht: { sky: 0.98, skyO: 0.6, skyK: 1.45, lit: 0.7, len: 1.35, shO: [0.75, 1], orb: [0.71, 0.11, 7], moon: true, rays: '', stars: 70, deer: 0, birds: false, glow: 0.45, tint: 0, hs: [0.74, 0.88, 0.82], ground: 0.8, label: 'bei Nacht mit Mondlicht' }
};

/** Animation start times and durations (SMIL clock values). */
export interface Timing {
	b0: string;
	b1: string;
	b2: string;
	b3: string;
	b4: string;
	stars: string;
	deer: string;
	light: string;
	amb: string;
	s1: string;
	s2: string;
	s3: string;
	s4: string;
	dur: string;
	slow: string;
	lightDur: string;
}

/** SVG path data and attributes of one scene. */
export interface Scene {
	width: number;
	height: number;
	label: string;
	// Sky: stipple, sun or moon, rays, stars.
	sky1: string;
	sky1o: number;
	sky2: string;
	stars: string;
	ox: number;
	oy: number;
	or: number;
	rays: string;
	moonH: string;
	// Two mountain ranges: area, outline, shading and its opacity.
	a0: string;
	o0: string;
	s0: string;
	sh0O: number;
	a1: string;
	o1: string;
	s1: string;
	sh1O: number;
	// Meadow in the foreground.
	mA: string;
	mO: string;
	mT: string;
	mTO: number;
	grass: string;
	// Pines: foliage, branches, highlights, bark.
	pF: string;
	pB: string;
	pHi: string;
	pBark: string;
	// House: hatching (light, medium, dark), position, chimney, window light.
	hL: string;
	hM: string;
	hD: string;
	hx: number;
	hy: number;
	chx: number;
	chy: number;
	/** Lit windows, or `null` for dark ones (drawn in ink). */
	win: string | null;
	glow: number;
	spill: number;
	glowV: string;
	// Deer: body, legs, highlights, head-down animation, placement.
	deerG: string;
	legs: string;
	deerHi: string;
	swap: string;
	d1: boolean;
	d1t: string;
	d2: boolean;
	d2t: string;
	/** Chimney smoke. */
	amb: boolean;
	birds: boolean;
	by: number;
	bv: string;
	bDur: string;
	tint: number;
	t: Timing;
}

/** Scene width for a box of `px` pixels: steps of 10, so resizing does not redraw on every pixel. */
export function sceneWidth(px: number): number {
	return Math.max(320, Math.round(px / 10) * 10);
}

export function landscape(zeit: TimeOfDay, width: number = SCENE_WIDTH, still = false): Scene {
	const W = width, H = SCENE_HEIGHT, TOP = 0.3, BASE = 0.86;
	// Density relative to the design width.
	const k = W / SCENE_WIDTH;
	const f = (n: number) => Math.round(n * 10) / 10;
	const mk = (seed: number) => {
		let s = seed;
		return () => {
			s = (s * 16807) % 2147483647;
			return (s - 1) / 2147483646;
		};
	};
	const geo = mk(5 * 7919 + 13), r2 = mk(20241), r3 = mk(4711);
	const M = MOODS[zeit];
	const ox = M.orb[0] * W, oy = M.orb[1] * H, orr = M.orb[2];
	const s = {
		width: W,
		height: H,
		label: 'Berglandschaft ' + M.label + ' mit Häuschen, Kiefern' + (M.deer ? ', grasenden Rehen' : '') + (M.birds ? ' und Vögeln' : '')
	} as Scene;

	// Sky: vertical dot stipple, thinning out towards the top, with a halo around sun or moon.
	const rr0 = orr + (M.moon ? 9 : 4);
	const vseg = (x: number, y0: number, y1: number) => {
		if (y1 - y0 < 1) return '';
		const dx = Math.abs(x - ox);
		if (dx < rr0) {
			const h = Math.sqrt(rr0 * rr0 - dx * dx), a = oy - h, b = oy + h;
			let d = '';
			if (a > y0) d += 'M' + f(x) + ' ' + f(y0) + 'V' + f(Math.min(a, y1));
			if (b < y1) d += 'M' + f(x) + ' ' + f(Math.max(b, y0)) + 'V' + f(y1);
			return d;
		}
		return 'M' + f(x) + ' ' + f(y0) + 'V' + f(y1);
	};
	let sky1 = '', sky2 = '';
	for (let x = 1; x < W; x += 2.4) {
		const yb = H * (TOP - 0.02) + H * 0.07 * Math.sin(x * 0.028 + 5) + H * 0.04 * Math.sin(x * 0.083 + 2);
		const y0 = r2() * H * 0.08;
		if (r2() < M.sky) sky1 += vseg(x, y0 + (1 - M.sky) * yb * 0.5 * r2(), yb);
		const kk = 0.5 + 0.5 * Math.sin(x * 0.045 + 3.5);
		const len = H * (0.04 + 0.2 * kk * (0.4 + 0.6 * r2())) * M.skyK;
		if (r2() < 0.35 + 0.65 * M.sky) sky2 += vseg(x, yb - len, yb);
	}
	s.sky1 = sky1;
	s.sky2 = sky2;
	s.sky1o = M.skyO;
	let stars = '';
	for (let i = 0; i < Math.round(M.stars * k); i++) {
		const x = r2() * W, y = r2() * H * TOP * 0.95;
		if (Math.hypot(x - ox, y - oy) > orr * 2.5) stars += 'M' + f(x) + ' ' + f(y) + 'h0.1';
	}
	s.stars = stars;
	s.ox = f(ox);
	s.oy = f(oy);
	s.or = orr;
	let rays = '', moonH = '';
	if (M.rays) {
		for (let a = 0; a < 360; a += 15) {
			const rad = (a * Math.PI) / 180, sy = Math.sin(rad);
			if (M.rays === 'up' && sy > 0.15) continue;
			const r0 = orr + 3, r1 = r0 + (a % 30 === 0 ? 9 : 5) * (M.rays === 'up' ? 1.3 : 0.8);
			rays += 'M' + f(ox + Math.cos(rad) * r0) + ' ' + f(oy + sy * r0) + 'L' + f(ox + Math.cos(rad) * r1) + ' ' + f(oy + sy * r1);
		}
	}
	if (M.moon) {
		for (let x = ox + 1.5; x < ox + orr; x += 1.1) {
			const h = Math.sqrt(Math.max(0, orr * orr - (x - ox) * (x - ox))) - 0.6;
			if (h > 0) moonH += 'M' + f(x) + ' ' + f(oy - h) + 'V' + f(oy + h);
		}
	}
	s.rays = rays;
	s.moonH = moonH;

	// Two mountain ranges with dashed shading.
	const N = Math.round(96 * k), R = BASE - TOP, bottom = H * BASE + 6;
	for (let L = 0; L < 2; L++) {
		const depth = L;
		const yBase = H * (TOP + R * (0.55 + 0.45 * depth));
		const amp = H * R * (0.78 - 0.26 * depth);
		const peaks: [number, number, number][] = [];
		const np = Math.round((3 + Math.floor(geo() * 3)) * k);
		for (let p = 0; p < np; p++) peaks.push([geo(), 0.45 + geo() * 0.55, (0.1 + geo() * 0.16) / k]);
		const pts: [number, number][] = [];
		for (let i = 0; i <= N; i++) {
			const x = (W * i) / N;
			let h = 0.08;
			peaks.forEach((pk) => {
				h = Math.max(h, pk[1] * (1 - Math.abs(x / W - pk[0]) / pk[2]));
			});
			h += (geo() - 0.5) * 0.1;
			pts.push([x, yBase - amp * Math.max(0, h)]);
		}
		const yAt = (x: number) => {
			const t = Math.max(0, Math.min(N - 0.001, (x / W) * N));
			const i = Math.floor(t);
			const u = t - i;
			return pts[i]![1] * (1 - u) + pts[i + 1]![1] * u;
		};
		const line = pts.map((p) => f(p[0]) + ' ' + f(p[1])).join('L');
		const area = 'M0 ' + f(bottom) + 'L' + line + 'L' + W + ' ' + f(bottom) + 'Z';
		let sh = '';
		for (let x = 0.5; x < W; x += 1.7 + (1 - depth) * 0.6) {
			const y = yAt(x), dark = yAt(x + 1.5) - yAt(x - 1.5) > 0.35;
			if (!dark && r3() > M.lit) continue;
			let len = dark ? (((6 + r3() * 26) * (0.6 + depth * 0.8) * H) / 260) * M.len : (((2 + r3() * 6) * H) / 260) * M.len;
			if (y + len > bottom) len = Math.max(0, bottom - y);
			sh += 'M' + f(x) + ' ' + f(y + 0.6) + 'l' + f(-len * 0.35) + ' ' + f(len);
		}
		for (let i = 2; i < N - 1; i++) {
			if (pts[i]![1] < pts[i - 1]![1] && pts[i]![1] < pts[i + 1]![1] && r3() < 0.75) {
				let x = pts[i]![0], y = pts[i]![1];
				sh += 'M' + f(x) + ' ' + f(y);
				for (let j = 0; j < 4; j++) {
					x += (r3() - 0.6) * 5;
					y += ((3 + r3() * 7) * H) / 260;
					if (y > bottom) break;
					sh += 'L' + f(x) + ' ' + f(y);
				}
			}
		}
		if (L === 0) {
			s.a0 = area;
			s.o0 = 'M' + line;
			s.s0 = sh;
			s.sh0O = M.shO[0];
		} else {
			s.a1 = area;
			s.o1 = 'M' + line;
			s.s1 = sh;
			s.sh1O = M.shO[1];
		}
	}

	// Meadow in the foreground.
	const mY = (x: number) => H * BASE + Math.sin(x * 0.021 + 0.6) * 2.4 + Math.sin(x * 0.07) * 0.8;
	let mo = '', mt = '', grass = '';
	for (let x = 0; x <= W; x += 3) mo += (x ? 'L' : 'M') + x + ' ' + f(mY(x));
	s.mA = mo + 'L' + W + ' ' + (H + 2) + 'L0 ' + (H + 2) + 'Z';
	s.mO = mo;
	for (let y0 = 2.2; y0 < H * (1 - BASE) + 4; y0 += 2.1) {
		let x = r3() * 6;
		while (x < W) {
			const len = 2 + r3() * 9;
			if (r3() < M.ground * (0.55 + y0 / 40)) mt += 'M' + f(x) + ' ' + f(mY(x) + y0) + 'h' + f(len);
			x += len + 2 + r3() * 8;
		}
	}
	for (let x = 1; x < W; x += 1.6) {
		if (r3() < 0.55) {
			const h = 1.2 + r3() * r3() * 4, lean = (r3() - 0.5) * 1.6, y = mY(x);
			grass += 'M' + f(x) + ' ' + f(y + 0.4) + 'q' + f(lean * 0.3) + ' ' + f(-h * 0.6) + ' ' + f(lean) + ' ' + f(-h);
		}
	}
	s.mT = mt;
	s.grass = grass;
	s.mTO = f(0.45 + 0.4 * M.ground);

	// Pines: slim, slightly leaning trunk, flat irregular crowns.
	let pF = '', pB = '', pHi = '', pBark = '';
	const blob = (cx: number, cy: number, rx: number, ry: number) => {
		let d = '';
		const n = 14;
		for (let i = 0; i < n; i++) {
			const a = (i / n) * Math.PI * 2, kk = 0.72 + r3() * 0.5, yk = Math.sin(a) < 0 ? 1 : 0.7;
			d += (i ? 'L' : 'M') + f(cx + Math.cos(a) * rx * kk) + ' ' + f(cy + Math.sin(a) * ry * kk * yk);
		}
		return d + 'Z';
	};
	const pine = (x: number, b: number, h: number, lean: number) => {
		const tw = Math.max(0.45, h * 0.022), tx = x + lean * h, ty = b - h * 0.86;
		const mx = (x + tx) / 2 + lean * h * 0.25, my = (b + ty) / 2;
		pF += 'M' + f(x - tw * 1.3) + ' ' + f(b + 1) + 'Q' + f(mx - tw) + ' ' + f(my) + ' ' + f(tx - tw * 0.4) + ' ' + f(ty) + 'L' + f(tx + tw * 0.4) + ' ' + f(ty) + 'Q' + f(mx + tw) + ' ' + f(my) + ' ' + f(x + tw * 1.3) + ' ' + f(b + 1) + 'Z';
		if (M.glow < 0.4) {
			for (let yy = b - 2; yy > b - h * 0.6; yy -= 2.4) {
				const u = (b - yy) / (b - ty), cx = x + (mx - x) * 2 * u * (1 - u) * 1 + (tx - x) * u * u;
				pBark += 'M' + f(cx - tw * 0.3) + ' ' + f(yy) + 'l' + f(tw * 0.5) + ' -0.8';
			}
		}
		const n = 3 + Math.floor(h / 14);
		for (let i = 0; i < n; i++) {
			const v = i / Math.max(1, n - 1), u = 0.55 + 0.42 * v;
			const sx = x + (tx - x) * u + lean * h * 0.1 * Math.sin(u * 3), sy = b - h * 0.86 * u;
			const side = i % 2 === 0 ? 1 : -1, reach = h * (0.1 + 0.14 * r3()) * (1.1 - 0.5 * v);
			const ex = sx + side * reach, ey = sy - reach * (0.25 + 0.3 * r3());
			pB += 'M' + f(sx) + ' ' + f(sy) + 'Q' + f(sx + side * reach * 0.5) + ' ' + f(sy - reach * 0.05) + ' ' + f(ex) + ' ' + f(ey);
			const rx = h * (0.09 + 0.06 * r3()) * (1.15 - 0.35 * v), ry = rx * (0.36 + 0.1 * r3());
			pF += blob(ex, ey, rx, ry);
			if (r3() < 0.6) pF += blob(ex - side * rx * 0.6, ey + ry * 0.4, rx * 0.7, ry * 0.9);
			if (M.glow < 0.4) pHi += 'M' + f(ex - rx * 0.7) + ' ' + f(ey - ry * 0.35) + 'q' + f(rx * 0.4) + ' ' + f(-ry * 0.5) + ' ' + f(rx * 0.9) + ' ' + f(-ry * 0.2);
		}
		const crx = h * 0.16, cry = crx * 0.42;
		pF += blob(tx, ty - cry * 0.4, crx, cry) + blob(tx + crx * 0.5, ty + cry * 0.2, crx * 0.65, cry * 0.85);
		if (M.glow < 0.4) pHi += 'M' + f(tx - crx * 0.75) + ' ' + f(ty - cry * 0.8) + 'q' + f(crx * 0.5) + ' ' + f(-cry * 0.6) + ' ' + f(crx * 1.1) + ' ' + f(-cry * 0.3);
	};
	// Groups at both edges stay at the edges; the single pine keeps its relative place.
	const right = W - SCENE_WIDTH;
	const pines: [number, number, number][] = [
		[6, 62, 0.06], [21, 47, -0.05], [34, 31, 0.08], [47, 19, -0.04],
		[384 + right, 64, -0.07], [369 + right, 49, 0.05], [355 + right, 34, -0.06], [342 + right, 21, 0.04],
		[118 * k, 13, 0.03]
	];
	pines.forEach((q) => pine(q[0], mY(q[0]) + 0.5, q[1], q[2]));
	s.pF = pF;
	s.pB = pB;
	s.pHi = pHi;
	s.pBark = pBark;

	// House.
	const hs = M.hs, hb: string[][] = [[], [], []];
	const hbin = (t: number) => (t < 0.35 ? 0 : t < 0.62 ? 1 : 2);
	for (let x = 0.7; x < 13.4; x += 1.2) {
		const top = x <= 7 ? -10 - (x / 7) * 6.6 : -10 - ((14 - x) / 7) * 6.6;
		hb[hbin(hs[0] + (r3() - 0.5) * 0.15)]!.push('M' + f(x) + ' ' + f(top + 0.5) + 'V-0.3');
	}
	for (let x = 14.6; x < 30.4; x += 1.05) {
		const top = -10 + (x - 14) / 16.6, bot = ((x - 14) / 16.6) * 0.5;
		hb[hbin(hs[1])]!.push('M' + f(x) + ' ' + f(top + 0.4) + 'V' + f(bot - 0.3));
	}
	if (hs[1] > 0.6) for (let y = -8.8; y < 0; y += 1.6) hb[1]!.push('M14.4 ' + f(y) + 'L30.4 ' + f(y + 0.6));
	for (let kk = 0.03; kk < 0.99; kk += 0.06) {
		hb[hbin(hs[2] + (kk > 0.7 ? 0.1 : 0))]!.push('M' + f(6.6 + 18 * kk) + ' ' + f(-17.4 + 0.9 * kk) + 'L' + f(14.6 + 17.6 * kk) + ' ' + f(-10.5 + 1.4 * kk));
	}
	s.hL = hb[0]!.join('');
	s.hM = hb[1]!.join('');
	s.hD = hb[2]!.join('');
	const hx = 0.62 * W;
	s.hx = f(hx);
	s.hy = f(mY(hx + 15) + 0.6);
	s.chx = f(hx + 21.8);
	s.chy = f(mY(hx + 15) + 0.6 - 20.8);
	s.win = M.glow > 0 ? LAMP : null;
	s.glow = M.glow;
	s.spill = f(M.glow * 0.6);
	s.glowV = still ? M.glow + ';' + M.glow : f(M.glow) + ';' + f(M.glow * 0.7) + ';' + f(M.glow * 1.1) + ';' + f(M.glow);

	// Deer, one grazing.
	const G = 'M6 12C10 9 22 9 27 11L31 17L34 24L36 26.5L35.2 28.4L33 28L31 24.5L28.2 19.5L27.5 20.5L27.8 30L26.6 30L25.6 21.2L14 21.2L12.4 30L11.2 30L10.6 21L8.6 20C6 18 5.2 14.5 6 12ZM33 23.4L30.6 20.8L32.2 23.8ZM6.2 12.4L4.6 14.2L5.8 14.6Z';
	const A = 'M6 12C10 9 22 9 27 11L28.6 5L29.4 2L30.4 4.6L33.6 6.2L35.2 7.6L34.4 8.8L30.6 8.6L29.2 13.5L27.5 20.5L27.8 30L26.6 30L25.6 21.2L14 21.2L12.4 30L11.2 30L10.6 21L8.6 20C6 18 5.2 14.5 6 12ZM28.8 4.2L26.6 1.8L28.4 3.2ZM6.2 12.4L4.6 14.2L5.8 14.6Z';
	s.deerG = G;
	s.legs = 'M24.2 21L23.2 30M12.2 21L9.8 30';
	s.deerHi = 'M9 12.4C13 10.8 20 10.6 25 11.8M8 15.5C12 14.5 18 14.4 23 15.2';
	s.swap = still ? [G, G, G, G].join(';') : [G, A, G, G].join(';');
	const sc = 0.46, dx1 = 0.44 * W, dx2 = 0.5 * W;
	s.d1t = 'translate(' + f(dx1) + ' ' + f(mY(dx1 + 9) - 30 * sc + 1.2) + ') scale(' + sc + ')';
	s.d2t = 'translate(' + f(dx2 + 40 * sc) + ' ' + f(mY(dx2 + 9) - 30 * sc + 1.2) + ') scale(' + -sc + ' ' + sc + ')';
	s.d1 = M.deer >= 1;
	s.d2 = M.deer >= 2;
	s.amb = !still;
	s.birds = M.birds && !still;
	s.by = f(H * 0.2);
	s.bv = '-40 ' + f(H * 0.2) + ';' + (W + 40) + ' ' + f(H * 0.13);
	// Same speed across any width.
	s.bDur = f((34 * (W + 80)) / (SCENE_WIDTH + 80)) + 's';
	s.tint = M.tint;

	const TT = { b0: 0.15, b1: 0.5, b2: 0.85, b3: 1.2, b4: 1.55, stars: 1.9, deer: 2.1, light: 2.3, amb: 2.6, s1: 2.7, s2: 4.0, s3: 5.3, s4: 6.6 };
	const t = {} as Timing;
	for (const [key, v] of Object.entries(TT)) t[key as keyof typeof TT] = still ? '0s' : v + 's';
	t.dur = still ? '0.01s' : '1.1s';
	t.slow = still ? '0.01s' : '1.6s';
	t.lightDur = still ? '0.01s' : '0.9s';
	s.t = t;
	return s;
}
