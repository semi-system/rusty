// The supply-chain gate of the front end tooling (design section 8). Build and test tools are the risk of this change, so every package that can run
// at build time is on a reviewed list:
//   node scripts/check-deps.mjs            fail unless package-lock.json is exactly deps.allow and every package passes the checks below
//   node scripts/check-deps.mjs --write    rewrite deps.allow from the lockfile (a deliberate, reviewed change: the diff is the list of new packages)
// Checks: every package comes from the npm registry with a sha512 integrity hash; none has an install script (installs run with --ignore-scripts anyway);
// every licence is on the allow-list; package.json pins exact versions (no ranges).
import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const lock = JSON.parse(readFileSync(join(root, 'package-lock.json'), 'utf8'));
const pkg = JSON.parse(readFileSync(join(root, 'package.json'), 'utf8'));
const allowPath = join(root, 'deps.allow');

// MPL-2.0 appears in build tools only (lightningcss); nothing under it is in the shipped bundle, which holds the compiled output of our code and Svelte's runtime (MIT).
const LICENCES = new Set(['MIT', 'ISC', 'Apache-2.0', 'BSD-2-Clause', 'BSD-3-Clause', '0BSD', 'BlueOak-1.0.0', 'MPL-2.0']);
const REGISTRY = 'https://registry.npmjs.org/';
// Optional, macOS-only file watching, never installed on Linux; installs run with --ignore-scripts regardless.
const INSTALL_SCRIPTS_OK = new Set(['fsevents@2.3.3']);

const problems = [];
const entries = [];
for (const [path, p] of Object.entries(lock.packages ?? {})) {
  if (path === '') continue;
  const name = path.slice(path.lastIndexOf('node_modules/') + 'node_modules/'.length);
  const id = `${name}@${p.version}`;
  entries.push(id);
  if (typeof p.resolved !== 'string' || !p.resolved.startsWith(REGISTRY)) problems.push(`${id}: not resolved from the npm registry`);
  if (typeof p.integrity !== 'string' || !p.integrity.startsWith('sha512-')) problems.push(`${id}: no sha512 integrity hash`);
  if (p.hasInstallScript && !INSTALL_SCRIPTS_OK.has(id)) problems.push(`${id}: has an install script`);
  const licence = typeof p.license === 'string' ? p.license : JSON.stringify(p.license);
  if (!LICENCES.has(licence)) problems.push(`${id}: licence ${licence} is not on the allow-list`);
}
for (const section of ['dependencies', 'devDependencies', 'optionalDependencies']) {
  for (const [name, range] of Object.entries(pkg[section] ?? {})) {
    if (!/^\d+\.\d+\.\d+$/.test(String(range))) problems.push(`package.json: ${name} is "${range}", not an exact version`);
  }
}
const current = [...new Set(entries)].sort();
const header = '# Every package of package-lock.json (name@version). A new one fails scripts/check-deps.mjs until this file is changed in a reviewed commit.\n';
if (process.argv.includes('--write')) {
  writeFileSync(allowPath, header + current.join('\n') + '\n');
  console.log(`deps.allow: ${current.length} packages`);
} else {
  let listed = [];
  try {
    listed = readFileSync(allowPath, 'utf8').split('\n').filter((l) => l !== '' && !l.startsWith('#'));
  } catch {
    problems.push('deps.allow is missing');
  }
  for (const id of current) if (!listed.includes(id)) problems.push(`${id}: not on deps.allow (add it in a reviewed change)`);
  for (const id of listed) if (!current.includes(id)) problems.push(`${id}: on deps.allow but not in the lockfile (remove it)`);
}
if (problems.length > 0) {
  console.error(problems.join('\n'));
  process.exit(1);
}
