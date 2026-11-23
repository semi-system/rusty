import { cleanup, fireEvent, render, screen } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import App from '../App.svelte';
import type { Account, Domain, Migration } from '../api/types';
import { App as State } from '../lib/app.svelte';
import { nav } from '../lib/nav.svelte';
import { en } from '../i18n/en';
import { setCatalogue, t } from '../i18n';
import { PLATFORM, fakeServer, ok, signedIn } from '../test-support';

beforeEach(() => setCatalogue(en, 'en'));
afterEach(cleanup);

const D: Domain = { id: 1, name: 'one.test', status: 'active' };
const amy: Account = { id: 6, address: 'amy@one.test', status: 'active', has_password: true, last_login_at: null, plan: null, used_bytes: 0, quota_bytes: null, last_delivery_at: null, must_change_password: false };
const run: Migration = { id: 4, host: 'imap.old.example', port: 993, security: 'tls', user: 'amy', requested_by: 'root@home.test', state: 'running', stopping: false, folder: 'INBOX', folders_total: 4, folders_done: 1, messages: 120, skipped: 2, bytes: 5_242_880, error: null, created_at: 1_700_000_000, finished_at: null };
const ACC = 'amy%40one.test';

function routes(extra: Record<string, unknown> = {}) {
  return {
    'GET /api/v1/domains': () => ok({ items: [D], next: null, total: 1 }),
    'GET /api/v1/domains/one.test/accounts': () => ok({ items: [amy], next: null, total: 1 }),
    'GET /api/v1/plans': () => ok({ items: [] }),
    ...extra,
  };
}

describe('moving a mailbox in from another IMAP server', () => {
  it('shows the progress of a move, asks for one with the login, and never sends the password anywhere but to the server', async () => {
    nav.go('/domains/one.test/accounts', true);
    let items: Migration[] = [{ ...run, state: 'done', folder: null, finished_at: 1_700_000_500, messages: 300 }];
    const s = fakeServer(
      signedIn(PLATFORM, routes({
        [`GET /api/v1/accounts/${ACC}/migrations`]: () => ok({ items }),
        [`POST /api/v1/accounts/${ACC}/migrations`]: () => {
          items = [{ ...run, id: 5, state: 'queued', messages: 0, skipped: 0, bytes: 0, folder: null }, ...items];
          return ok(items[0]);
        },
      }) as never),
    );
    render(App, { app: new State(s.client) });
    await fireEvent.click(await screen.findByRole('button', { name: t('migrate.open', { address: 'amy@one.test' }) }));
    expect(await screen.findByText(/300 messages copied, 2 skipped/)).toBeTruthy();
    // Nothing is sent without the login and the password.
    await fireEvent.click(screen.getByRole('button', { name: t('migrate.start') }));
    expect(await screen.findByText(t('migrate.invalid'))).toBeTruthy();
    expect(s.last(`POST /api/v1/accounts/${ACC}/migrations`)).toBeUndefined();
    await fireEvent.input(screen.getByLabelText(t('migrate.host')), { target: { value: ' imap.old.example ' } });
    await fireEvent.input(screen.getByLabelText(t('migrate.user')), { target: { value: 'amy' } });
    await fireEvent.input(screen.getByLabelText(t('migrate.password')), { target: { value: 'old-pass' } });
    await fireEvent.click(screen.getByRole('button', { name: t('migrate.start') }));
    expect(await screen.findByText(t('migrate.asked'))).toBeTruthy();
    expect(s.last(`POST /api/v1/accounts/${ACC}/migrations`)?.body).toEqual({ host: 'imap.old.example', port: 993, security: 'tls', user: 'amy', password: 'old-pass' });
    expect((screen.getByLabelText(t('migrate.password')) as HTMLInputElement).value).toBe('');
    expect(await screen.findByText(t('migrate.state.queued'), { exact: false })).toBeTruthy();
  });
});
