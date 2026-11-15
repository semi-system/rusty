import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { describe, expect, it } from 'vitest';
// @ts-expect-error a plain ES module without declarations
import { generate, tsType } from './gen-types.mjs';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const contract = JSON.parse(readFileSync(join(root, '..', '..', 'crates', 'rusty-admin', 'contract', 'v1.json'), 'utf8'));

describe('the generated types', () => {
  it('are exactly what the contract file says (regenerate with `node scripts/gen-types.mjs`)', () => {
    expect(readFileSync(join(root, 'src', 'api', 'types.ts'), 'utf8')).toBe(generate(contract));
  });

  it('map the contract notations', () => {
    const shapes = { Domain: {} };
    expect(tsType('integer', shapes)).toBe('number');
    expect(tsType('string|null', shapes)).toBe('string | null');
    expect(tsType('string[]', shapes)).toBe('string[]');
    expect(tsType('Domain[]', shapes)).toBe('Domain[]');
    expect(tsType('integer|null', shapes)).toBe('number | null');
    expect(tsType('string|null (optional)', shapes)).toBe('string | null');
    expect(() => tsType('mystery', shapes)).toThrow();
  });
});
