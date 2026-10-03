// Account setup, login with authenticator app and passkey, administration, browsing files – in a
// real browser.
import { createHash, createHmac, randomBytes } from 'node:crypto';
import { existsSync, readFileSync } from 'node:fs';
import { mkdir, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
import { expect, test, type Browser, type Page } from '@playwright/test';

const PASSWORD = 'Wolken über dem Garten 7';

function base32Decode(s: string): Buffer {
	const a = 'ABCDEFGHIJKLMNOPQRSTUVWXYZ234567';
	let bits = 0;
	let value = 0;
	const out: number[] = [];
	for (const c of s.replace(/\s/g, '')) {
		value = (value << 5) | a.indexOf(c);
		bits += 5;
		if (bits >= 8) {
			out.push((value >>> (bits - 8)) & 0xff);
			bits -= 8;
		}
	}
	return Buffer.from(out);
}

function totp(secret: Buffer, step: number): string {
	const msg = Buffer.alloc(8);
	msg.writeBigUInt64BE(BigInt(step));
	const h = createHmac('sha1', secret).update(msg).digest();
	const o = h[h.length - 1] & 0x0f;
	const bin = ((h[o] & 0x7f) << 24) | (h[o + 1] << 16) | (h[o + 2] << 8) | h[o + 3];
	return String(bin % 1_000_000).padStart(6, '0');
}

const step = () => Math.floor(Date.now() / 1000 / 30);

async function virtualAuthenticator(page: Page) {
	const cdp = await page.context().newCDPSession(page);
	await cdp.send('WebAuthn.enable');
	await cdp.send('WebAuthn.addVirtualAuthenticator', {
		options: {
			protocol: 'ctap2',
			transport: 'internal',
			hasResidentKey: true,
			hasUserVerification: true,
			isUserVerified: true,
			automaticPresenceSimulation: true
		}
	});
}

/** 8×8 pixels, blue. */
const PNG = 'iVBORw0KGgoAAAANSUhEUgAAAAgAAAAICAIAAABLbSncAAAAEUlEQVR4nGNQTX6NFTEMLQkADGRcwcht3uAAAAAASUVORK5CYII=';

const PDF =
	'%PDF-1.4\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n' +
	'3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 200]>>endobj\ntrailer<</Root 1 0 R>>\n%%EOF\n';

/** Signs out: account and settings, then "Abmelden" at the bottom (as in the design). */
async function signOut(page: Page) {
	await page.getByRole('link', { name: 'Konto und Einstellungen' }).click();
	await expect(page.getByRole('heading', { name: 'Einstellungen' })).toBeVisible();
	await page.getByRole('button', { name: 'Abmelden', exact: true }).last().click();
	await expect(page.getByRole('heading', { name: 'Anmelden' })).toBeVisible();
}

/** The start page greets by time of day. */
const GREETING = /^(Guten (Morgen|Tag|Abend), .+|Noch wach, .+\?)$/;

/**
 * Connects a device like the Mac app would: the sign-in page opens in the browser, the person
 * signs in and allows the device, the app exchanges the code with its PKCE verifier.
 */
async function connectDevice(page: Page) {
	const verifier = 'e2e-verifier-0123456789-abcdefghijklmnopqrstuvwxyz';
	const challenge = createHash('sha256').update(verifier).digest('base64url');
	const query = new URLSearchParams({ challenge, redirect_uri: 'xlrx://auth', state: 'e2e', name: 'Testmac', platform: 'macos' });

	// Signed out: first the sign-in, then back to the device page.
	await signOut(page);
	await page.goto(`/device?${query}`);
	await expect(page).toHaveURL(/\/login\?next=/);
	await page.getByLabel('Benutzername').fill('admin');
	await page.getByRole('button', { name: 'Mit Passkey anmelden' }).click();
	await expect(page.getByRole('heading', { name: 'Gerät verbinden' })).toBeVisible();
	await expect(page.getByText('Testmac (Mac) möchte auf deine Dateien zugreifen.')).toBeVisible();
	await page.getByRole('button', { name: 'Verbinden' }).click();
	const back = page.getByRole('link', { name: 'Zurück zur App' });
	await expect(back).toBeVisible();
	const redirect = new URL((await back.getAttribute('href'))!);
	expect(`${redirect.protocol}//${redirect.host}`).toBe('xlrx://auth');
	expect(redirect.searchParams.get('state')).toBe('e2e');

	// The app's side.
	const res = await page.request.post('/api/devices/token', {
		data: { grant_type: 'authorization_code', code: redirect.searchParams.get('code'), code_verifier: verifier, redirect_uri: 'xlrx://auth' }
	});
	expect(res.status()).toBe(200);
	const tokens = await res.json();
	const auth = { Authorization: `Bearer ${tokens.access_token}` };
	expect((await page.request.get('/api/roots', { headers: auth })).status()).toBe(200);

	// Listed under "Sicherheit", signed out from there. In a new tab: in this one the browser still
	// asks which app should open `xlrx://` (there is none here) and takes no clicks.
	const tab = await page.context().newPage();
	await tab.goto('/settings/security');
	const row = tab.getByRole('listitem').filter({ hasText: 'Testmac' });
	await expect(row).toBeVisible();
	await row.getByRole('button', { name: 'Abmelden' }).click();
	await tab.getByRole('alertdialog').getByRole('button', { name: 'Gerät abmelden' }).click();
	await expect(tab.getByText('Noch keine Geräte.')).toBeVisible();
	expect((await tab.request.get('/api/roots', { headers: auth })).status()).toBe(401);
	await tab.close();
}

/**
 * A file over 8 MiB goes in parts: a lost part is sent again on its own, and after a refused part
 * "Fortsetzen" sends only what is missing.
 */
async function uploadLarge(page: Page, data: string) {
	const MIB = 1024 * 1024;
	const content = randomBytes(18 * MIB);
	const seen: number[] = [];
	let lose = true;
	let refuse = true;
	await page.route(/\/api\/uploads\/[^/]+\/parts\?offset=\d+$/, async (route) => {
		const offset = Number(new URL(route.request().url()).searchParams.get('offset'));
		seen.push(offset);
		if (offset === 8 * MIB && lose) {
			lose = false;
			return route.abort('failed');
		}
		if (offset === 16 * MIB && refuse) {
			refuse = false;
			return route.fulfill({ status: 400, contentType: 'application/json', body: '{"error":"Testfehler"}' });
		}
		return route.continue();
	});
	await page.getByRole('link', { name: 'Dateien', exact: true }).click();
	await page.getByTestId('upload-input').setInputFiles({ name: 'Gross.bin', mimeType: 'application/octet-stream', buffer: content });
	await expect(page.getByText('Testfehler')).toBeVisible({ timeout: 15_000 });
	await page.getByRole('button', { name: 'Fortsetzen' }).click();
	await expect(page.getByRole('link', { name: 'Gross.bin', exact: true })).toBeVisible({ timeout: 15_000 });
	await page.unroute(/\/api\/uploads\//);
	expect(seen).toEqual([0, 8 * MIB, 8 * MIB, 16 * MIB, 16 * MIB]);
	expect(readFileSync(join(data, 'homes/admin/Drive/Gross.bin')).equals(content)).toBe(true);
	await page.getByRole('button', { name: 'Uploads schließen' }).click();
}

/** Browses "My Drive" of the signed-in admin: folders, preview, download, rescan. */
async function browseFiles(page: Page, data: string) {
	const drive = join(data, 'homes/admin/Drive');
	await mkdir(join(drive, 'Projekte'), { recursive: true });
	await mkdir(join(drive, 'Bilder'), { recursive: true });
	await writeFile(join(drive, 'Projekte/Plan.txt'), 'Erste Zeile\nZweite Zeile: äöü\n');
	await writeFile(join(drive, 'Projekte/seite.html'), '<script>alert("xss")</script>');
	await writeFile(join(drive, 'Bilder/Punkt.png'), Buffer.from(PNG, 'base64'));
	await writeFile(join(drive, 'Bericht.pdf'), PDF);
	// While user content is shown: no script of it may open a dialog.
	const scriptRan = () => {
		throw new Error('Benutzerinhalt hat ein Skript ausgeführt');
	};
	page.on('dialog', scriptRan);
	// Anything the browser refuses (CSP, framing) is a bug in the headers.
	const refused: string[] = [];
	page.on('console', (m) => {
		if (/refused|content security policy/i.test(m.text())) refused.push(m.text());
	});

	// The first visit imports the existing folder; the page shows the content as it arrives.
	await page.getByRole('link', { name: 'Dateien', exact: true }).click();
	await expect(page.getByRole('heading', { name: 'Meine Ablage' })).toBeVisible();
	await expect(page.getByRole('link', { name: 'Projekte', exact: true })).toBeVisible();

	// Image preview: loads under the strict CSP of the app.
	await page.getByRole('link', { name: 'Bilder', exact: true }).click();
	// The list shows a thumbnail of the picture (loaded, not the icon).
	const thumb = page.getByRole('link', { name: 'Punkt.png', exact: true }).locator('img.thumb');
	await expect(thumb).toBeVisible();
	await expect.poll(() => thumb.evaluate((i: HTMLImageElement) => i.complete && i.naturalWidth)).toBe(8);
	await page.getByRole('link', { name: 'Punkt.png', exact: true }).click();
	const img = page.getByRole('img', { name: 'Punkt.png' });
	await expect(img).toBeVisible();
	await expect.poll(() => img.evaluate((i: HTMLImageElement) => i.naturalWidth)).toBe(8);

	// Back via the breadcrumbs; text preview and download.
	await page.getByRole('navigation', { name: 'Pfad' }).getByRole('link', { name: 'Meine Ablage' }).click();
	await page.getByRole('link', { name: 'Projekte', exact: true }).click();
	await page.getByRole('link', { name: 'Plan.txt', exact: true }).click();
	await expect(page.locator('pre')).toContainText('Zweite Zeile: äöü');
	await expect(page.getByRole('link', { name: 'In neuem Tab' })).toBeVisible();
	const download = page.waitForEvent('download');
	await page.getByRole('link', { name: 'Herunterladen', exact: true }).click();
	expect((await download).suggestedFilename()).toBe('Plan.txt');

	// HTML is never shown, let alone run.
	await page.getByRole('navigation', { name: 'Pfad' }).getByRole('link', { name: 'Projekte', exact: true }).click();
	await page.getByRole('link', { name: 'seite.html', exact: true }).click();
	await expect(page.getByText('Keine Vorschau für diesen Dateityp.')).toBeVisible();
	await expect(page.getByRole('link', { name: 'In neuem Tab' })).toHaveCount(0);

	// A file added on the NAS (SMB, File Station) shows up by itself within seconds: watcher,
	// journal, live event, reload of the view.
	await page.getByRole('navigation', { name: 'Pfad' }).getByRole('link', { name: 'Projekte', exact: true }).click();
	await expect(page.getByRole('heading', { name: 'Projekte' })).toBeVisible();
	await writeFile(join(drive, 'Projekte/Neu.txt'), 'neu');
	await expect(page.getByRole('link', { name: 'Neu.txt', exact: true })).toBeVisible({ timeout: 10_000 });
	// "Neu einlesen" finds nothing left to do.
	await page.getByRole('button', { name: 'Neu einlesen' }).click();
	await expect(page.getByText('Keine Änderungen.')).toBeVisible();

	// PDF: shown in a frame of the app.
	await page.getByRole('navigation', { name: 'Pfad' }).getByRole('link', { name: 'Meine Ablage' }).click();
	// (Not "networkidle": the live event stream keeps the connection open.)
	const pdf = page.waitForResponse((r) => r.url().includes('/content?inline=true'));
	await page.getByRole('link', { name: 'Bericht.pdf', exact: true }).click();
	const frame = page.locator('iframe[title="Bericht.pdf"]');
	await expect(frame).toBeVisible();
	expect((await pdf).status()).toBe(200);
	await page.waitForTimeout(500);
	expect(refused).toEqual([]);
	page.off('dialog', scriptRan);
}

/** Changes through the web app: folder, upload with conflicts, versions, rename, move, trash. */
async function changeFiles(page: Page, data: string) {
	const drive = join(data, 'homes/admin/Drive');
	const nav = () => page.getByRole('navigation', { name: 'Pfad' });
	const link = (name: string) => page.getByRole('link', { name, exact: true });
	const menu = async (name: string, action: string) => {
		await page.getByRole('button', { name: `Aktionen für ${name}` }).click();
		await page.getByRole('menuitem', { name: action }).click();
	};
	const upload = async (name: string, content: string) => {
		await page.getByTestId('upload-input').setInputFiles({ name, mimeType: 'text/plain', buffer: Buffer.from(content) });
	};

	await page.getByRole('link', { name: 'Dateien', exact: true }).click();
	await page.getByRole('button', { name: 'Neuer Ordner' }).click();
	await page.getByLabel('Name').fill('Belege');
	await page.getByRole('button', { name: 'Anlegen' }).click();
	await link('Belege').click();
	await expect(page.getByRole('heading', { name: 'Belege' })).toBeVisible();

	// Changes from elsewhere (another device, SMB) appear without reloading the page.
	const url = new URL(page.url());
	const here = url.pathname.split('/').pop();
	const created = await page.request.post(`/api/nodes/${here}/folders`, {
		data: { name: 'Von woanders' },
		headers: { origin: url.origin }
	});
	expect(created.status()).toBe(201);
	await expect(link('Von woanders')).toBeVisible();

	// Upload; the same name again: keep both, then replace (the old content becomes a version).
	await upload('Rechnung.txt', 'Rechnung 1');
	await expect(link('Rechnung.txt')).toBeVisible();
	await upload('Rechnung.txt', 'Rechnung 2');
	await page.getByRole('button', { name: 'Beide behalten' }).click();
	await expect(link('Rechnung (1).txt')).toBeVisible();
	await upload('rechnung.TXT', 'Rechnung 3');
	await page.getByRole('button', { name: 'Ersetzen' }).click();
	await expect(page.getByText('Uploads abgeschlossen')).toBeVisible();
	await page.getByRole('button', { name: 'Uploads schließen' }).click();
	await link('Rechnung.txt').click();
	await expect(page.locator('pre')).toHaveText('Rechnung 3');
	await page.getByRole('tab', { name: /Versionen/ }).click();
	const versions = page.locator('section.versions li.old');
	await expect(versions).toHaveCount(1);
	await versions.getByRole('button', { name: 'Wiederherstellen' }).click();
	await expect(page.locator('pre')).toHaveText('Rechnung 1');
	await expect(versions).toHaveCount(2);

	// Rename and move.
	await nav().getByRole('link', { name: 'Belege' }).click();
	await menu('Rechnung (1).txt', 'Umbenennen');
	await page.getByLabel('Name').fill('Quittung.txt');
	await page.getByRole('button', { name: 'Fertig' }).click();
	await expect(link('Quittung.txt')).toBeVisible();
	await menu('Quittung.txt', 'Verschieben');
	const dialog = page.getByRole('dialog');
	await dialog.getByRole('button', { name: 'Meine Ablage' }).click();
	await dialog.getByRole('button', { name: 'Projekte' }).click();
	await dialog.getByRole('button', { name: 'In „Projekte“ verschieben' }).click();
	await expect(page.getByText('„Quittung.txt“ wurde verschoben.')).toBeVisible();
	await expect(link('Quittung.txt')).toHaveCount(0);
	expect(readFileSync(join(drive, 'Projekte/Quittung.txt'), 'utf8')).toBe('Rechnung 2');

	// Delete, undo, delete again, restore from the trash.
	await menu('Rechnung.txt', 'In den Papierkorb');
	await page.getByRole('alertdialog').getByRole('button', { name: 'In den Papierkorb' }).click();
	await expect(page.getByText('„Rechnung.txt“ liegt jetzt im Papierkorb.')).toBeVisible();
	expect(existsSync(join(drive, 'Belege/Rechnung.txt'))).toBe(false);
	await page.getByRole('button', { name: 'Rückgängig' }).click();
	await expect(link('Rechnung.txt')).toBeVisible();
	await menu('Rechnung.txt', 'In den Papierkorb');
	await page.getByRole('alertdialog').getByRole('button', { name: 'In den Papierkorb' }).click();
	await page.getByRole('link', { name: 'Papierkorb', exact: true }).click();
	const row = page.getByRole('listitem').filter({ hasText: 'Rechnung.txt' });
	await expect(row).toContainText('Meine Ablage/Belege');
	await row.getByRole('button', { name: 'Wiederherstellen' }).click();
	await expect(page.getByText('„Rechnung.txt“ ist wieder da.')).toBeVisible();
	await expect(page.getByText('Der Papierkorb ist leer.')).toBeVisible();
	expect(readFileSync(join(drive, 'Belege/Rechnung.txt'), 'utf8')).toBe('Rechnung 1');
}

/** Search: content of a file put on the NAS, word stems, kinds, suggestions, recent, folder. */
async function searchFiles(page: Page, data: string) {
	const drive = join(data, 'homes/admin/Drive');
	await mkdir(join(drive, 'Notizen'), { recursive: true });
	await writeFile(join(drive, 'Notizen/Heizung.txt'), 'Die Wärmepumpe im Keller wurde gewartet.\n');
	const field = page.getByRole('combobox', { name: 'Suchen', exact: true });
	const link = (name: string) => page.getByRole('link', { name, exact: true });

	await page.getByRole('link', { name: 'Suche', exact: true }).click();
	await expect(field).toBeFocused();
	// Found by its content once the watcher saw it and the text was read.
	await expect(async () => {
		await field.fill('Wärmepumpe');
		await field.press('Enter');
		await expect(link('Heizung.txt')).toBeVisible({ timeout: 1000 });
	}).toPass({ timeout: 20_000 });
	await expect(page).toHaveURL(/\/search\?q=W%C3%A4rmepumpe/);
	await expect(page.locator('.hits mark')).toHaveText('Wärmepumpe');
	await expect(page.getByText('Meine Ablage › Notizen')).toBeVisible();

	// Word stems; counts per kind, and choosing one.
	await field.fill('Rechnungen');
	await field.press('Enter');
	await expect(link('Rechnung.txt')).toBeVisible();
	await expect(link('Quittung.txt')).toBeVisible();
	const texts = page.getByRole('button', { name: /^Texte/ });
	await texts.click();
	await expect(texts).toHaveAttribute('aria-pressed', 'true');
	await expect(page.getByText(/Treffer · Texte/)).toBeVisible();

	// While typing: names, chosen with the keyboard.
	await field.fill('Heiz');
	const option = page.getByRole('listbox', { name: 'Dateinamen' }).getByRole('option', { name: /Heizung\.txt/ });
	await expect(option).toBeVisible();
	await expect(option.locator('strong')).toHaveText('Heiz');
	await field.press('ArrowDown');
	await expect(option).toHaveAttribute('aria-selected', 'true');
	await field.press('Enter');
	await expect(page.getByRole('heading', { name: 'Heizung.txt' })).toBeVisible();

	// Recent searches stay in this browser.
	await page.getByRole('link', { name: 'Suche', exact: true }).click();
	await expect(page.getByRole('button', { name: 'Wärmepumpe' })).toBeVisible();
	await page.getByRole('button', { name: 'Löschen' }).click();
	await expect(page.getByRole('button', { name: 'Wärmepumpe' })).toHaveCount(0);

	// Only within a folder.
	await page.getByRole('link', { name: 'Dateien', exact: true }).click();
	await link('Projekte').click();
	await page.getByRole('link', { name: 'In diesem Ordner suchen' }).click();
	await expect(page.getByText('In „Projekte“')).toBeVisible();
	await field.fill('Zeile');
	await field.press('Enter');
	await expect(link('Plan.txt')).toBeVisible();
	await field.fill('Wärmepumpe');
	await field.press('Enter');
	await expect(page.getByText('Nichts gefunden.')).toBeVisible();
	await page.getByRole('button', { name: 'Überall suchen' }).click();
	await expect(link('Heizung.txt')).toBeVisible();
}

/** Sets up an account from its setup link in a browser of its own (password + authenticator app). */
async function setupAccount(browser: Browser, url: string): Promise<Page> {
	const page = await (await browser.newContext()).newPage();
	await page.goto(url);
	await page.getByLabel('Neues Passwort').fill(PASSWORD);
	await page.getByLabel('Passwort wiederholen').fill(PASSWORD);
	await page.getByRole('button', { name: 'Weiter' }).click();
	await page.getByRole('button', { name: 'Authenticator-App einrichten' }).click();
	const secret = base32Decode(await page.locator('p.muted .code').innerText());
	await page.getByLabel('Angezeigter Code').fill(totp(secret, step()));
	await page.getByRole('button', { name: 'Bestätigen' }).click();
	await page.getByLabel('Ich habe die Codes sicher gespeichert.').check();
	await page.getByRole('button', { name: 'Fertig' }).click();
	await expect(page.getByRole('link', { name: 'Konto und Einstellungen' })).toBeVisible();
	return page;
}

/** Sharing a folder with another person: what they see, what they may do, and ending it. */
async function shareFiles(page: Page, browser: Browser, bertSetup: string) {
	const link = (p: Page, name: string) => p.getByRole('link', { name, exact: true });
	const bert = await setupAccount(browser, bertSetup);
	await bert.getByRole('link', { name: 'Geteilt', exact: true }).first().click();
	await expect(bert.getByText('Noch nichts.')).toBeVisible();

	// The owner shares "Projekte" to edit.
	await page.getByRole('link', { name: 'Dateien', exact: true }).click();
	await page.getByRole('button', { name: 'Aktionen für Projekte' }).click();
	await page.getByRole('menuitem', { name: 'Teilen' }).click();
	const sheet = page.getByRole('dialog', { name: '„Projekte“ teilen' });
	await sheet.getByLabel('Person oder Gruppe').selectOption({ label: 'Bert' });
	await sheet.getByLabel('Rolle', { exact: true }).selectOption('editor');
	await sheet.getByRole('button', { name: 'Teilen', exact: true }).click();
	await expect(sheet.getByLabel('Rolle von Bert')).toHaveValue('editor');
	await sheet.getByRole('button', { name: 'Schließen' }).click();
	await link(page, 'Projekte').click();
	await expect(page.getByText(/geteilt mit Bert/)).toBeVisible();

	// Bert finds it under "Geteilt", sees only it and may add to it.
	await bert.reload();
	await expect(link(bert, 'Projekte')).toBeVisible();
	await link(bert, 'Projekte').click();
	await expect(bert.getByRole('heading', { name: 'Projekte' })).toBeVisible();
	await expect(bert.getByRole('navigation', { name: 'Pfad' }).getByRole('link', { name: 'Geteilt' })).toBeVisible();
	await expect(bert.getByRole('navigation', { name: 'Pfad' }).getByRole('link', { name: 'Meine Ablage' })).toHaveCount(0);
	await expect(link(bert, 'Plan.txt')).toBeVisible();
	await bert.getByTestId('upload-input').setInputFiles({ name: 'Von Bert.txt', mimeType: 'text/plain', buffer: Buffer.from('Hallo') });
	await expect(link(bert, 'Von Bert.txt')).toBeVisible();
	// The owner sees it arrive by itself.
	await expect(link(page, 'Von Bert.txt')).toBeVisible();
	// Nothing else of the owner's is reachable.
	const rootId = new URL(page.url()).pathname.split('/').pop();
	const parent = await page.request.get(`/api/nodes/${rootId}`).then((r) => r.json());
	await bert.goto(`/files/${parent.path[0].id}`);
	await expect(bert.getByText('Nicht gefunden – vielleicht gelöscht oder verschoben.')).toBeVisible();

	// Ended by the owner: gone for Bert.
	await page.getByRole('button', { name: 'Teilen' }).first().click();
	await page.getByRole('dialog').getByRole('button', { name: 'Freigabe für Bert beenden' }).click();
	await expect(page.getByRole('dialog').getByText('Noch mit niemandem geteilt.')).toBeVisible();
	await page.getByRole('dialog').getByRole('button', { name: 'Schließen' }).click();
	await bert.goto('/shared');
	await expect(bert.getByText('Noch nichts.')).toBeVisible();
	await bert.context().close();
}

test('Einrichtung, Anmeldung, Verwaltung und Dateien', async ({ page, browser }) => {
	const setupUrl = process.env.XLRX_SETUP_URL;
	test.skip(!setupUrl, 'XLRX_SETUP_URL fehlt (e2e/run.sh verwenden)');
	await virtualAuthenticator(page);

	// Setup from the link: password, authenticator app, recovery codes.
	await page.goto(setupUrl!);
	await expect(page.getByRole('heading', { name: 'Konto einrichten' })).toBeVisible();
	await expect(page).toHaveURL(/\/setup$/); // token removed from the address bar
	await page.getByLabel('Neues Passwort').fill(PASSWORD);
	await page.getByLabel('Passwort wiederholen').fill(PASSWORD);
	await page.getByRole('button', { name: 'Weiter' }).click();
	await page.getByRole('button', { name: 'Authenticator-App einrichten' }).click();
	const secret = base32Decode(await page.locator('p.muted .code').innerText());
	await page.getByLabel('Angezeigter Code').fill(totp(secret, step()));
	await page.getByRole('button', { name: 'Bestätigen' }).click();
	await expect(page.getByRole('heading', { name: 'Wiederherstellungscodes' })).toBeVisible();
	await expect(page.locator('.codes span')).toHaveCount(10);
	await page.getByLabel('Ich habe die Codes sicher gespeichert.').check();
	await page.getByRole('button', { name: 'Fertig' }).click();
	await expect(page.getByRole('heading', { name: GREETING })).toBeVisible();
	await expect(page.getByRole('img', { name: /^Berglandschaft/ })).toBeVisible();

	// Add a passkey (the login counts as a fresh second factor).
	await page.getByRole('link', { name: 'Konto und Einstellungen' }).click();
	await page.getByPlaceholder('Name, z.B. MacBook').fill('Testgerät');
	await page.getByRole('button', { name: 'Passkey hinzufügen' }).click();
	await expect(page.getByText('Testgerät', { exact: true })).toBeVisible();

	// Log out, log in with the passkey alone.
	await signOut(page);
	await page.getByLabel('Benutzername').fill('admin');
	await page.getByRole('button', { name: 'Mit Passkey anmelden' }).click();
	await expect(page.getByRole('heading', { name: GREETING })).toBeVisible();

	// Log out, log in with password + code (next time step: the setup code must not work again).
	await signOut(page);
	await page.getByLabel('Benutzername').fill('admin');
	await page.getByLabel('Passwort').fill(PASSWORD);
	await page.getByRole('button', { name: 'Weiter' }).click();
	await page.getByLabel('Code aus der Authenticator-App').fill(totp(secret, step() + 1));
	await page.getByRole('button', { name: 'Anmelden' }).click();
	await expect(page.getByRole('heading', { name: GREETING })).toBeVisible();

	// Administration: new account with a setup link.
	await page.getByRole('link', { name: 'Verwaltung' }).click();
	await page.getByLabel('Benutzername').fill('bert');
	await page.getByLabel('Anzeigename').fill('Bert');
	await page.getByRole('button', { name: /Anlegen/ }).click();
	await expect(page.getByText('Einrichtungslink für bert')).toBeVisible();
	await expect(page.locator('input[readonly]')).toHaveValue(/\/setup#/);
	const bertSetup = await page.locator('input[readonly]').inputValue();
	await expect(page.getByRole('cell', { name: 'user_created' }).first()).toBeVisible();

	const data = process.env.XLRX_E2E_DATA;
	if (data) {
		await browseFiles(page, data);
		await changeFiles(page, data);
		await searchFiles(page, data);
		await shareFiles(page, browser, bertSetup);
		await uploadLarge(page, data);
	}
	await connectDevice(page);
});

test('Falsches Passwort zeigt einheitliche Meldung', async ({ page }) => {
	await page.goto('/login');
	await page.getByLabel('Benutzername').fill('gibtsnicht');
	await page.getByLabel('Passwort').fill('falsch falsch falsch');
	await page.getByRole('button', { name: 'Weiter' }).click();
	await expect(page.getByRole('alert')).toHaveText('Benutzername oder Passwort ist falsch.');
});

test('Landschaft folgt der Tageszeit', async ({ page }) => {
	await page.clock.install({ time: new Date(2026, 9, 2, 20, 59, 30) });
	await page.goto('/login');
	const scene = page.getByRole('img', { name: /^Berglandschaft/ });
	await expect(scene).toHaveAccessibleName('Berglandschaft am Abend mit Häuschen, Kiefern, grasenden Rehen und Vögeln');
	// Smoke and birds move.
	await expect(scene.locator('animateTransform')).not.toHaveCount(0);

	// The page stays open past 21:00: night falls without a reload.
	await page.clock.fastForward('01:00');
	await expect(scene).toHaveAccessibleName('Berglandschaft bei Nacht mit Mondlicht mit Häuschen, Kiefern');

	// Reduced motion: nothing moves any more.
	await page.emulateMedia({ reducedMotion: 'reduce' });
	await expect(scene.locator('animateTransform')).toHaveCount(0);
});
