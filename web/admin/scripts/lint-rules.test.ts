import { ESLint } from 'eslint';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { beforeAll, describe, expect, it } from 'vitest';

const cwd = join(dirname(fileURLToPath(import.meta.url)), '..');
const eslint = new ESLint({ cwd });

async function messages(code: string, file = 'src/pages/Probe.svelte'): Promise<string[]> {
  const [r] = await eslint.lintText(code, { filePath: join(cwd, file) });
  return (r?.messages ?? []).map((m) => `${m.ruleId}: ${m.message}`);
}

const page = (body: string) => `<script lang="ts">\n  import { t } from '../i18n';\n  const x = t('app.title');\n</script>\n${body}\n`;

describe('the lint rules that carry design decisions', () => {
  // The first lint of a run loads every plugin (slow on a network or WSL file system): pay for it once, here.
  beforeAll(async () => {
    await messages('export {};', 'src/lib/warm.ts');
  }, 180000);

  it('accept a template that only uses the catalogue', async () => {
    expect(await messages(page('<main><h1>{x}</h1><button aria-label={x}>{t("nav.signOut")}</button></main>'))).toEqual([]);
  });

  it('forbid {@html} (nothing is rendered as markup)', async () => {
    const m = await messages(page('<div>{@html x}</div>'));
    expect(m.some((x) => x.startsWith('svelte/no-at-html-tags'))).toBe(true);
  });

  it('forbid a literal text in a template: it must be a catalogue key', async () => {
    const m = await messages(page('<p>Hello world</p>'));
    expect(m.some((x) => x.startsWith('no-restricted-syntax') && x.includes('message catalogue'))).toBe(true);
  });

  it('forbid a literal accessible name, title and placeholder', async () => {
    for (const attr of ['aria-label="Close the dialog"', 'title="Close"', 'placeholder="Name"', 'alt="A picture"']) {
      const m = await messages(page(`<input ${attr} />`));
      expect(m.some((x) => x.startsWith('no-restricted-syntax')), attr).toBe(true);
    }
  });

  it('forbid eval and the Function constructor', async () => {
    const ev = await messages('export const a = eval("1");\n', 'src/lib/probe.ts');
    expect(ev.some((x) => x.startsWith('no-eval'))).toBe(true);
    const fn = await messages('export const b = new Function("return 1");\n', 'src/lib/probe.ts');
    expect(fn.some((x) => x.startsWith('no-new-func'))).toBe(true);
  });
});
