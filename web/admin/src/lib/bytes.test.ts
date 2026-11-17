import { describe, expect, it } from 'vitest';
import { bytes, megabytes } from './bytes';

describe('bytes', () => {
  it('says a size in the largest unit that keeps it above one', () => {
    expect(bytes(0)).toBe('0 B');
    expect(bytes(512)).toBe('512 B');
    expect(bytes(1536)).toBe('1.5 KB');
    expect(bytes(5 * 1024 * 1024 * 1024)).toBe('5 GB');
  });
  it('reads megabytes from a form field', () => {
    expect(megabytes('')).toBeNull();
    expect(megabytes(' 2 ')).toBe(2_097_152);
    expect(Number.isNaN(megabytes('abc'))).toBe(true);
    expect(Number.isNaN(megabytes('0'))).toBe(true);
    expect(Number.isNaN(megabytes('-3'))).toBe(true);
  });
});
