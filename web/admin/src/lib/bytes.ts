/** A count of bytes in words: `0 B`, `512 KB`, `1.5 GB` (binary units, one decimal at most). */
export function bytes(n: number): string {
  const units = ['B', 'KB', 'MB', 'GB', 'TB', 'PB'];
  let v = n;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i += 1;
  }
  const text = i === 0 || v >= 100 ? String(Math.round(v)) : String(Math.round(v * 10) / 10);
  return `${text} ${units[i]}`;
}

/** Megabytes typed in a form as a count of bytes; `null` for an empty field, `NaN` for something that is not a positive number. */
export function megabytes(text: string): number | null {
  const t = text.trim();
  if (t === '') return null;
  const n = Number(t);
  return Number.isFinite(n) && n > 0 ? Math.round(n * 1_048_576) : Number.NaN;
}
