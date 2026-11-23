import js from '@eslint/js';
import tseslint from 'typescript-eslint';
import globals from 'globals';

// The rules that carry design decisions, not taste:
//  * `innerHTML` and `dangerouslySetInnerHTML`-style sinks are forbidden (the compiler escapes text; nothing is rendered as markup);
//  * a person-readable string is never a literal in a template or in an accessibility attribute: it is a key of the message catalogue.
const RESTRICTED = [
  {
    selector: 'JSXAttribute[name.name=/^(innerHTML|outerHTML|textContent)$/]',
    message: 'Nothing is rendered as markup: set text through the compiler, never innerHTML.',
  },
  {
    selector: 'JSXText[value=/[A-Za-z]{2,}/]',
    message: 'A text in a template must come from the message catalogue: use {t("key")}.',
  },
  {
    selector: 'JSXAttribute[name.name=/^(aria-label|aria-description|placeholder|title|alt)$/] > Literal[value=/[A-Za-z]{2,}/]',
    message: 'An accessible name or tooltip must come from the message catalogue: use {t("key")}.',
  },
  {
    selector: 'AssignmentExpression[left.property.name=/^(innerHTML|outerHTML)$/]',
    message: 'Nothing is rendered as markup: never assign innerHTML.',
  },
];

export default tseslint.config(
  { ignores: ['dist', 'node_modules'] },
  js.configs.recommended,
  ...tseslint.configs.recommended,
  {
    languageOptions: { globals: { ...globals.browser, ...globals.node } },
    rules: {
      'no-eval': 'error',
      'no-new-func': 'error',
      'no-implied-eval': 'error',
    },
  },
  {
    files: ['src/**/*.tsx', 'src/**/*.ts'],
    ignores: ['src/**/*.test.ts', 'src/**/*.test.tsx'],
    rules: { 'no-restricted-syntax': ['error', ...RESTRICTED] },
  },
);
