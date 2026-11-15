import js from '@eslint/js';
import svelte from 'eslint-plugin-svelte';
import tseslint from 'typescript-eslint';
import globals from 'globals';

// The rules that carry design decisions, not taste:
//  * `{@html}` is forbidden (design section 6: the compiler escapes text; nothing is rendered as markup);
//  * a person-readable string is never a literal in a template or in an accessibility attribute: it is a key of the message catalogue (decision Q6).
const NO_LITERAL_TEXT = [
  {
    selector: 'SvelteText[value=/[A-Za-z]{2,}/]',
    message: 'A text in a template must come from the message catalogue: use {t("key")}.',
  },
  {
    selector: 'SvelteAttribute[key.name=/^(aria-label|aria-description|aria-placeholder|title|placeholder|alt)$/] > SvelteLiteral[value=/[A-Za-z]{2,}/]',
    message: 'An accessible name or tooltip must come from the message catalogue: use {t("key")}.',
  },
];

export default tseslint.config(
  { ignores: ['dist', 'node_modules'] },
  js.configs.recommended,
  ...tseslint.configs.recommended,
  ...svelte.configs['flat/recommended'],
  {
    languageOptions: { globals: { ...globals.browser, ...globals.node } },
    rules: {
      'svelte/no-at-html-tags': 'error',
      'no-eval': 'error',
      'no-new-func': 'error',
      'no-implied-eval': 'error',
    },
  },
  {
    files: ['**/*.svelte'],
    languageOptions: { parserOptions: { parser: tseslint.parser } },
    rules: { 'no-restricted-syntax': ['error', ...NO_LITERAL_TEXT] },
  },
  {
    files: ['**/*.svelte.ts'],
    languageOptions: { parser: tseslint.parser },
  },
);
