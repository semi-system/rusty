// Generates src/api/types.ts from the contract file of the admin service (crates/rusty-admin/contract/v1.json).
// `node scripts/gen-types.mjs` writes the file; `node scripts/gen-types.mjs --check` fails when the committed file is out of date.
import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const contractPath = join(here, '..', '..', '..', 'crates', 'rusty-admin', 'contract', 'v1.json');
const outPath = join(here, '..', 'src', 'api', 'types.ts');

const PRIMITIVES = { integer: 'number', number: 'number', string: 'string', boolean: 'boolean' };

export function tsType(t, shapes) {
  const text = String(t).split(' ')[0] ?? '';
  if (text.endsWith('|null')) return `${tsType(text.slice(0, -5), shapes)} | null`;
  if (text.endsWith('[]')) {
    const inner = tsType(text.slice(0, -2), shapes);
    return inner.includes(' ') ? `(${inner})[]` : `${inner}[]`;
  }
  if (text in PRIMITIVES) return PRIMITIVES[text];
  if (text in shapes) return text;
  throw new Error(`unknown type ${t}`);
}

export function generate(contract) {
  const lines = ['// Generated from crates/rusty-admin/contract/v1.json by scripts/gen-types.mjs. Do not edit: change the contract and regenerate.', ''];
  for (const [name, fields] of Object.entries(contract.shapes)) {
    const body = Object.entries(fields).map(([k, t]) => `  ${/^[A-Za-z_]\w*$/.test(k) ? k : JSON.stringify(k)}: ${tsType(t, contract.shapes)};`);
    const exported = name === 'Error' ? 'ErrorBody' : name;
    lines.push(body.length ? `export interface ${exported} {\n${body.join('\n')}\n}` : `export type ${exported} = Record<string, never>;`, '');
  }
  return lines.join('\n');
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const text = generate(JSON.parse(readFileSync(contractPath, 'utf8')));
  if (process.argv.includes('--check')) {
    let current = '';
    try {
      current = readFileSync(outPath, 'utf8');
    } catch {
      // missing counts as out of date
    }
    if (current !== text) {
      console.error('src/api/types.ts is out of date: run `node scripts/gen-types.mjs`');
      process.exit(1);
    }
  } else {
    writeFileSync(outPath, text);
  }
}
