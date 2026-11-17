// A design-review server: the built console (dist/) with a fake API behind it, so every page can be looked at, and photographed, without a database.
//   npm run build && node scripts/preview.mjs [--port 4173] [--scope platform|domain]
// Nothing here is the real API and nothing is stored; the data is a small, believable installation. Not part of the product or of the build.
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { dirname, extname, join, normalize } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..', 'dist');
const arg = (name, fallback) => {
  const i = process.argv.indexOf(`--${name}`);
  return i > 0 ? process.argv[i + 1] : fallback;
};
const port = Number(arg('port', '4173'));
const scope = arg('scope', 'platform');
const state = arg('state', 'in'); // in | signin | setup
const me = { address: scope === 'platform' ? 'ops@rusty.lan' : 'admin@example.test', scope, domain: scope === 'platform' ? null : 1, csrf: 'c'.repeat(64) };

const now = Math.floor(Date.now() / 1000);
const domains = [
  { id: 1, name: 'example.test', status: 'active' },
  { id: 2, name: 'rusty.lan', status: 'active' },
  { id: 3, name: 'retired.example', status: 'disabled' },
];
const dns = (d) => ({
  warnings: [],
  records: [
    { name: d, type: 'MX', value: `10 mail.rusty.lan.`, ttl: 3600, purpose: 'Where mail for the domain is delivered', required: 'required', status: 'ok', seen: null, last_checked_at: now - 600 },
    { name: d, type: 'TXT', value: 'v=spf1 ip4:192.168.1.204 -all', ttl: 3600, purpose: 'Who may send mail for the domain', required: 'required', status: 'ok', seen: null, last_checked_at: now - 600 },
    { name: `rusty._domainkey.${d}`, type: 'TXT', value: 'v=DKIM1; k=ed25519; p=MCowBQYDK2VwAyEAq3J0m0b1p8qXWk4Ndf0o7cS0e1kQvY3r2m5nD8tT6xA=', ttl: 3600, purpose: 'Signature key of the outgoing mail', required: 'required', status: 'missing', seen: null, last_checked_at: now - 600 },
    { name: `_dmarc.${d}`, type: 'TXT', value: `v=DMARC1; p=quarantine; rua=mailto:postmaster@${d}`, ttl: 3600, purpose: 'What receivers do with mail that fails the checks', required: 'recommended', status: 'mismatch', seen: 'v=DMARC1; p=none', last_checked_at: now - 600 },
    { name: `autoconfig.${d}`, type: 'CNAME', value: 'mail.rusty.lan.', ttl: 3600, purpose: 'Lets Thunderbird find the settings by itself', required: 'optional', status: 'unchecked', seen: null, last_checked_at: null },
  ],
});
const api = {
  'GET /api/v1/session': () => me,
  'GET /api/v1/setup': () => ({ required: state === 'setup' }),
  'GET /api/v1/domains': () => ({ items: scope === 'platform' ? domains : domains.slice(0, 1), next: null }),
  'GET /api/v1/monitor': () => ({
    services: ['admin', 'edge', 'gc', 'imap', 'outbound', 'smtp-in', 'submission'].map((s, i) => ({
      service: s,
      status: s === 'outbound' ? 'degraded' : 'ok',
      instances: [{ instance: `rusty_${s}.1.k${i}x9`, version: '0.1.0', uptime_secs: 86400 + i * 3700, age_secs: 4, stale: false, status: s === 'outbound' ? 'degraded' : 'ok', listeners: s === 'smtp-in' || s === 'imap' ? 2 : null }],
    })),
    queue: { queued: 3, running: 1, failed: 2, dead: 0, oldest_due_secs: 840 },
    storage: [{ status: 'ok', count: 1 }, { status: 'unchecked', count: 1 }],
    dns_problem_domains: 1,
  }),
};
const per = {
  outbound: () => ({ queued: 3, running: 1, failed: 2, dead: 0 }),
  dns: (d) => dns(d),
  accounts: (d) => ({
    items: [
      { id: 10, address: `alice@${d}`, status: 'active', has_password: true },
      { id: 11, address: `bob@${d}`, status: 'active', has_password: true },
      { id: 12, address: `old.mailbox@${d}`, status: 'disabled', has_password: false },
    ],
    next: null,
  }),
  aliases: (d) => ({ items: [{ id: 1, alias: `postmaster@${d}`, target: `alice@${d}` }, { id: 2, alias: `abuse@${d}`, target: `alice@${d}` }], next: null }),
  dkim: (d) => ({ items: [{ selector: 'rusty', algorithm: 'ed25519', record_name: `rusty._domainkey.${d}`, record_value: 'v=DKIM1; k=ed25519; p=MCowBQYDK2VwAyEAq3J0m0b1p8qXWk4Ndf0o7cS0e1kQvY3r2m5nD8tT6xA=', sealed: true }] }),
  relay: () => ({ relay: { host: 'smtp.relay.example', port: 587, tls: 'starttls', username: 'mailer', spf_include: '_spf.relay.example', password_set: true, check_status: 'ok', checked_at: now - 3600 } }),
  storage: () => ({
    storage: { kind: 's3', ready: false, check_status: 'unchecked', checked_at: null, settings: { endpoint: 'https://s3.example', region: 'us-east-1', bucket: 'mail', prefix: '', path_style: false }, access_key_set: true, secret_key_set: true, policy: null, backends: null },
  }),
};

const types = { '.html': 'text/html; charset=utf-8', '.js': 'text/javascript', '.css': 'text/css', '.svg': 'image/svg+xml', '.woff2': 'font/woff2' };
createServer(async (req, res) => {
  const url = new URL(req.url ?? '/', 'http://x');
  const json = (v) => {
    res.writeHead(200, { 'content-type': 'application/json' });
    res.end(JSON.stringify(v));
  };
  if (url.pathname.startsWith('/api/')) {
    if (state !== 'in' && url.pathname === '/api/v1/session') {
      res.writeHead(401, { 'content-type': 'application/json' });
      return res.end(JSON.stringify({ error: 'unauthorized' }));
    }
    const exact = api[`${req.method} ${url.pathname}`];
    if (exact) return json(exact());
    const m = /^\/api\/v1\/domains\/([^/]+)\/([a-z]+)$/.exec(url.pathname);
    if (m && req.method === 'GET' && per[m[2]]) return json(per[m[2]](decodeURIComponent(m[1])));
    res.writeHead(404, { 'content-type': 'application/json' });
    return res.end(JSON.stringify({ error: 'not_found' }));
  }
  const file = normalize(join(root, url.pathname)).startsWith(root) && extname(url.pathname) ? join(root, url.pathname) : join(root, 'index.html');
  try {
    const body = await readFile(file);
    res.writeHead(200, { 'content-type': types[extname(file)] ?? 'application/octet-stream' });
    res.end(body);
  } catch {
    res.writeHead(404);
    res.end();
  }
}).listen(port, '0.0.0.0', () => console.log(`preview on http://localhost:${port} (${scope})`));
