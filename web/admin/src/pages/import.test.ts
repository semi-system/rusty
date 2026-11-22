import { cleanup, fireEvent, render, screen } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import App from '../App.svelte';
import type { Domain } from '../api/types';
import { App as State } from '../lib/app.svelte';
import { nav } from '../lib/nav.svelte';
import { en } from '../i18n/en';
import { setCatalogue, t } from '../i18n';
import { PLATFORM, fakeServer, ok, signedIn } from '../test-support';

beforeEach(() => {
  setCatalogue(en, 'en');
  nav.go('/domains/one.test/accounts', true);
});
afterEach(cleanup);

const D: Domain = { id: 1, name: 'one.test', status: 'active' };

describe('import', () => {
  it('checks the lines first, then makes the accounts, and says which lines were wrong', async () => {
    const s = fakeServer(
      signedIn(PLATFORM, {
        'GET /api/v1/domains': () => ok({ items: [D], next: null, total: 1 }),
        'GET /api/v1/domains/one.test/accounts': () => ok({ items: [], next: null, total: 0 }),
        'GET /api/v1/plans': () => ok({ items: [] }),
        'POST /api/v1/domains/one.test/accounts/import': (body) => {
          const b = body as { csv: string; dry_run: boolean };
          return ok({ dry_run: b.dry_run, created: 2, exists: 0, invalid: 1, rows: [{ line: 3, address: 'x@one.test', result: 'invalid', reason: 'the password is too short' }] });
        },
      }),
    );
    render(App, { app: new State(s.client) });
    const box = await screen.findByLabelText(t('import.text'));
    await fireEvent.input(box, { target: { value: 'alice,alice-password-1\nbob,bob-password-22\nx,short' } });
    await fireEvent.click(screen.getByRole('button', { name: t('import.check') }));
    expect(await screen.findByText(t('import.checked', { n: 2, exists: 0, bad: 1 }))).toBeTruthy();
    expect(s.last('POST /api/v1/domains/one.test/accounts/import')?.body).toEqual({ csv: 'alice,alice-password-1\nbob,bob-password-22\nx,short', dry_run: true });
    expect(screen.getByText(t('import.line', { line: 3, address: 'x@one.test', reason: 'the password is too short' }))).toBeTruthy();
    await screen.findByRole('button', { name: t('import.run') });
    await fireEvent.click(screen.getByRole('button', { name: t('import.run') }));
    expect(await screen.findByText(t('import.done', { n: 2, exists: 0, bad: 1 }))).toBeTruthy();
  });
});
