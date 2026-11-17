import { cleanup, fireEvent, render, screen } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import App from '../App.svelte';
import type { Account, Domain } from '../api/types';
import { App as State } from '../lib/app.svelte';
import { nav } from '../lib/nav.svelte';
import { en } from '../i18n/en';
import { setCatalogue, t } from '../i18n';
import { PLATFORM, fakeServer, fail, ok, signedIn } from '../test-support';

beforeEach(() => setCatalogue(en, 'en'));
afterEach(cleanup);

const D: Domain = { id: 1, name: 'one.test', status: 'active' };
const closed: Account = { id: 5, address: 'bob@one.test', status: 'disabled', has_password: true, last_login_at: null, plan: null, used_bytes: 0, quota_bytes: null };
const open: Account = { ...closed, id: 6, address: 'amy@one.test', status: 'active' };

describe('deleting', () => {
  it('offers the delete of an account only when it is closed, and asks before it does it', async () => {
    nav.go('/domains/one.test/accounts', true);
    const s = fakeServer(
      signedIn(PLATFORM, {
        'GET /api/v1/domains': () => ok({ items: [D], next: null, total: 1 }),
        'GET /api/v1/domains/one.test/accounts': () => ok({ items: [closed, open], next: null, total: 2 }),
        'GET /api/v1/plans': () => ok({ items: [] }),
        'DELETE /api/v1/accounts/bob%40one.test': () => ok({ deletion: 'scheduled' }),
      }),
    );
    render(App, { app: new State(s.client) });
    const button = await screen.findByRole('button', { name: t('accounts.delete', { address: 'bob@one.test' }) });
    expect(screen.queryByRole('button', { name: t('accounts.delete', { address: 'amy@one.test' }) }), 'an open account has no delete').toBeNull();
    await fireEvent.click(button);
    expect(s.calls.some((c) => c.key.startsWith('DELETE'))).toBe(false);
    await fireEvent.click(await screen.findByRole('button', { name: t('accounts.deleteYes') }));
    expect(await screen.findByText(t('accounts.deleteScheduled', { address: 'bob@one.test' }))).toBeTruthy();
  });

  it('says why a domain that is not empty is not deleted', async () => {
    nav.go('/', true);
    const s = fakeServer(
      signedIn(PLATFORM, {
        'GET /api/v1/domains': () => ok({ items: [D], next: null, total: 1 }),
        'DELETE /api/v1/domains/one.test': () => fail(409, 'not_empty'),
      }),
    );
    render(App, { app: new State(s.client) });
    await fireEvent.click(await screen.findByRole('button', { name: t('domains.delete', { name: 'one.test' }) }));
    await fireEvent.click(await screen.findByRole('button', { name: t('domains.deleteYes') }));
    expect(await screen.findByText(t('error.not_empty'))).toBeTruthy();
  });
});
