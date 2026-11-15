import { en, type Catalogue, type MessageKey } from './en';

export type { MessageKey } from './en';

let catalogue: Catalogue = en;
let language = 'en';

/** Replace the active catalogue (a language file, or the pseudo-locale of the tests). */
export function setCatalogue(c: Catalogue, lang: string): void {
  catalogue = c;
  language = lang;
  if (typeof document !== 'undefined') {
    document.documentElement.lang = lang;
  }
}

export function currentLanguage(): string {
  return language;
}

/** `t("setup.tooShort", { min: 12 })`: the message of `key` with `{name}` placeholders filled in. A placeholder without a value is left visible. */
export function t(key: MessageKey, params: Record<string, string | number> = {}): string {
  const message = catalogue[key];
  return message.replace(/\{(\w+)\}/g, (whole, name: string) => {
    const v = params[name];
    return v === undefined ? whole : String(v);
  });
}

/** A pseudo-locale for the tests: every message is wrapped and lengthened, so a hard-coded string stands out and a truncated layout shows. */
export function pseudo(c: Catalogue): Catalogue {
  const out = {} as Record<MessageKey, string>;
  for (const key of Object.keys(c) as MessageKey[]) {
    const accent = c[key].replace(/\{\w+\}|[aeiou]/g, (m) => (m.startsWith('{') ? m : m + '́'));
    out[key] = `[${accent}~~~]`;
  }
  return out;
}
