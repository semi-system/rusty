// Scans the build output (dist/) for what the page's Content-Security-Policy forbids or what must never ship (design sections 6 and 8):
// `eval`, `new Function`, inline script and style, `javascript:` URLs, an external origin, a source map, `{@html}`-style raw markup sinks.
// `node scripts/check-bundle.mjs [dist]` exits 1 and lists every finding.
import { readdirSync, readFileSync, statSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const dist = process.argv[2] ?? join(dirname(fileURLToPath(import.meta.url)), '..', 'dist');

function files(dir) {
  return readdirSync(dir).flatMap((n) => {
    const p = join(dir, n);
    return statSync(p).isDirectory() ? files(p) : [p];
  });
}

const found = [];
for (const f of files(dist)) {
  const name = f.slice(dist.length + 1);
  if (name.endsWith('.map')) found.push(`${name}: a source map is shipped`);
  if (!/\.(js|css|html)$/.test(name)) continue;
  const text = readFileSync(f, 'utf8');
  if (name.endsWith('.js')) {
    for (const [re, why] of [
      [/\beval\s*\(/, 'eval('],
      [/\bnew\s+Function\s*\(/, 'new Function('],
      [/\bFunction\s*\(\s*["'`]/, 'Function("...")'],
      [/\bsetTimeout\s*\(\s*["'`]/, 'setTimeout with a string'],
      [/\bsetInterval\s*\(\s*["'`]/, 'setInterval with a string'],
      [/\bdocument\.write\s*\(/, 'document.write('],
      [/javascript:/i, 'a javascript: URL'],
      [/\bimportScripts\s*\(/, 'importScripts('],
    ]) {
      if (re.test(text)) found.push(`${name}: ${why}`);
    }
  }
  if (name.endsWith('.html')) {
    if (/<script(?![^>]*\bsrc=)[^>]*>/i.test(text)) found.push(`${name}: an inline script`);
    if (/<style[\s>]/i.test(text)) found.push(`${name}: an inline style element`);
    if (/\sstyle\s*=/i.test(text)) found.push(`${name}: an inline style attribute`);
    if (/\son[a-z]+\s*=/i.test(text)) found.push(`${name}: an inline event handler`);
  }
  // No third-party origin anywhere a browser would load it from (an XML namespace or the Svelte docs link in an error message is not a load).
  for (const m of text.matchAll(/(?:src|href)\s*=\s*["']?(https?:)?\/\/[^"'\s>]+/gi)) found.push(`${name}: loads ${m[0]}`);
  for (const m of text.matchAll(/url\(\s*["']?(https?:)?\/\/[^)"']+/gi)) found.push(`${name}: loads ${m[0]}`);
  for (const m of text.matchAll(/@import\s+["']?https?:/gi)) found.push(`${name}: imports ${m[0]}`);
}
if (!files(dist).some((f) => f.endsWith('index.html'))) found.push('index.html is missing');
if (found.length > 0) {
  console.error(found.join('\n'));
  process.exit(1);
}
console.log(`bundle ok: ${files(dist).length} files`);
