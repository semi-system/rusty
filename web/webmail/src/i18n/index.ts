import { en, type Catalogue, type MessageKey } from './en';

export type { MessageKey } from './en';

let catalogue: Catalogue = en;

/** Replace the active catalogue (a language file, or the pseudo-locale of the tests). */
export function setCatalogue(c: Catalogue, lang: string): void {
  catalogue = c;
  if (typeof document !== 'undefined') {
    document.documentElement.lang = lang;
  }
}

/** `t("compose.quote", { from })`: the message of `key` with `{name}` placeholders filled in. A placeholder without a value stays visible. */
export function t(key: MessageKey, params: Record<string, string | number> = {}): string {
  return catalogue[key].replace(/\{(\w+)\}/g, (whole, name: string) => {
    const v = params[name];
    return v === undefined ? whole : String(v);
  });
}

/** The message for a key made from a word the server sent (`error.` + code): `fallback` when the catalogue has no such key. */
export function tFor(prefix: string, value: string, fallback: MessageKey): string {
  const key = `${prefix}${value}`;
  return t(key in en ? (key as MessageKey) : fallback);
}

/** A pseudo-locale for the tests: every message is wrapped, so a hard-coded string stands out. */
export function pseudo(c: Catalogue): Catalogue {
  const out = {} as Record<MessageKey, string>;
  for (const key of Object.keys(c) as MessageKey[]) {
    out[key] = `[${c[key]}~~]`;
  }
  return out;
}
