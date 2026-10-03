// A stand-in for the AI provider in the end-to-end test (OpenAI API on 127.0.0.1:8015).
// Embeddings are bags of words; a few words mean the same ("Therme" is a heating), so a text can
// be found by what it means without containing the word searched for.
import { createServer } from 'node:http';

const SYNONYMS = { therme: 'heizung', heizkessel: 'heizung', ferien: 'urlaub' };

function stem(w) {
	w = w.toLowerCase();
	if (SYNONYMS[w]) return SYNONYMS[w];
	for (const s of ['en', 'er', 'es', 'e', 'n', 's']) if ([...w].length > 4 && w.endsWith(s)) return w.slice(0, -s.length);
	return w;
}

function fnv(s) {
	let h = 0xcbf29ce484222325n;
	for (const b of Buffer.from(s)) h = BigInt.asUintN(64, (h ^ BigInt(b)) * 0x100000001b3n);
	return h;
}

function embed(text, dim) {
	const t = text.split('Query: ').pop().replace(/^(query|passage): /, '');
	const v = new Array(dim).fill(0);
	for (const w of t.split(/[^\p{L}\p{N}]+/u).filter(Boolean)) v[Number(fnv(stem(w)) % BigInt(dim))] += 1;
	v[dim - 1] += 0.01;
	return v;
}

createServer((req, res) => {
	let body = '';
	req.on('data', (c) => (body += c));
	req.on('end', () => {
		const send = (code, json) => {
			res.writeHead(code, { 'content-type': 'application/json' });
			res.end(JSON.stringify(json));
		};
		if (req.headers.authorization !== 'Bearer e2e-schluessel') return send(401, { error: { message: 'kein Zugang' } });
		const b = JSON.parse(body || '{}');
		if (req.url === '/v1/embeddings') {
			const input = Array.isArray(b.input) ? b.input : [b.input];
			const dim = b.dimensions ?? 256;
			const tokens = input.reduce((n, t) => n + t.split(/\s+/).length, 0);
			return send(200, {
				object: 'list',
				data: input.map((t, index) => ({ object: 'embedding', index, embedding: embed(t, dim) })),
				usage: { prompt_tokens: tokens, total_tokens: tokens }
			});
		}
		if (req.url === '/v1/chat/completions') {
			const answer = { beschreibung: 'Ein Foto.', tags: ['foto'], text_im_bild: '', dokumenttyp: null, datum_erkannt: null };
			return send(200, {
				choices: [{ index: 0, message: { role: 'assistant', content: JSON.stringify(answer) } }],
				usage: { prompt_tokens: 280, completion_tokens: 30 }
			});
		}
		send(404, { error: { message: 'unbekannt' } });
	});
}).listen(8015, '127.0.0.1');
