import { cleanup, fireEvent, render, screen } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import App from '../App.svelte';
import type { Account, Domain, Transfer } from '../api/types';
import { App as State } from '../lib/app.svelte';
import { nav } from '../lib/nav.svelte';
import { en } from '../i18n/en';
import { setCatalogue, t } from '../i18n';
import { PLATFORM, TENANT, fakeServer, ok, signedIn } from '../test-support';

beforeEach(() => setCatalogue(en, 'en'));
afterEach(cleanup);

const D: Domain = { id: 1, name: 'one.test', status: 'active' };
const amy: Account = { id: 6, address: 'amy@one.test', status: 'active', has_password: true, last_login_at: null, plan: null, used_bytes: 0, quota_bytes: null, last_delivery_at: null, must_change_password: false };
const base: Transfer = { id: 7, kind: 'export', requested_by: 'root@home.test', state: 'done', folder: null, size: 2048, messages: 3, skipped: 0, error: null, created_at: 1700000000, finished_at: 1700000010, expires_at: 1700086400 };
const ACC = 'amy%40one.test';

function routes(extra: Record<string, unknown> = {}) {
  return {
    'GET /api/v1/domains': () => ok({ items: [D], next: null, total: 1 }),
    'GET /api/v1/domains/one.test/accounts': () => ok({ items: [amy], next: null, total: 1 }),
    'GET /api/v1/plans': () => ok({ items: [] }),
    ...extra,
  };
}

describe('mailbox export and import', () => {
  it('asks for an export, lists it, and links the download of a finished one', async () => {
    nav.go('/domains/one.test/accounts', true);
    let items: Transfer[] = [base];
    const s = fakeServer(
      signedIn(PLATFORM, routes({
        [`GET /api/v1/accounts/${ACC}/transfers`]: () => ok({ items }),
        [`POST /api/v1/accounts/${ACC}/exports`]: () => {
          items = [{ ...base, id: 8, state: 'queued', finished_at: null, messages: 0, size: 0 }, base];
          return ok({ ...base, id: 8, state: 'queued' });
        },
      }) as never),
    );
    render(App, { app: new State(s.client) });
    await fireEvent.click(await screen.findByRole('button', { name: t('transfers.open', { address: 'amy@one.test' }) }));
    const link = (await screen.findByRole('link', { name: t('transfers.download') })) as HTMLAnchorElement;
    expect(link.getAttribute('href')).toBe('/api/v1/transfers/7/download');
    await fireEvent.click(screen.getByRole('button', { name: t('transfers.export') }));
    expect(await screen.findByText(t('transfers.exportAsked'))).toBeTruthy();
    expect(s.calls.some((c) => c.key === `POST /api/v1/accounts/${ACC}/exports`)).toBe(true);
    expect(await screen.findByText(t('transfers.state.queued'), { exact: false })).toBeTruthy();
  });

  it('uploads a file in pieces of 48 KiB and then starts the import', async () => {
    nav.go('/domains/one.test/accounts', true);
    const s = fakeServer(
      signedIn(PLATFORM, routes({
        [`GET /api/v1/accounts/${ACC}/transfers`]: () => ok({ items: [] }),
        [`POST /api/v1/accounts/${ACC}/imports`]: () => ok({ ...base, id: 9, kind: 'import', state: 'uploading', folder: 'Archive' }),
        'PUT /api/v1/transfers/9/chunks/0': () => ok({}),
        'PUT /api/v1/transfers/9/chunks/1': () => ok({}),
        'PUT /api/v1/transfers/9/chunks/2': () => ok({}),
        'POST /api/v1/transfers/9/start': () => ok({ ...base, id: 9, kind: 'import', state: 'queued' }),
      }) as never),
    );
    render(App, { app: new State(s.client) });
    await fireEvent.click(await screen.findByRole('button', { name: t('transfers.open', { address: 'amy@one.test' }) }));
    await fireEvent.input(await screen.findByLabelText(t('transfers.folder')), { target: { value: 'Archive' } });
    // 100 000 bytes are three pieces of at most 49 152.
    const file = new File([new Uint8Array(100_000)], 'old.mbox');
    await fireEvent.change(screen.getByLabelText(t('transfers.import')), { target: { files: [file] } });
    expect(await screen.findByText(t('transfers.importAsked'))).toBeTruthy();
    expect(s.last(`POST /api/v1/accounts/${ACC}/imports`)?.body).toEqual({ folder: 'Archive' });
    expect(s.calls.filter((c) => c.key.startsWith('PUT /api/v1/transfers/9/chunks/')).map((c) => c.key)).toEqual([0, 1, 2].map((n) => `PUT /api/v1/transfers/9/chunks/${n}`));
    expect(s.last('POST /api/v1/transfers/9/start')?.body).toEqual({ pieces: 3 });
  });

  it('is for platform administrators: a domain administrator is offered no such button', async () => {
    nav.go('/domains/one.test/accounts', true);
    const s = fakeServer(signedIn(TENANT, routes() as never));
    render(App, { app: new State(s.client) });
    await screen.findByText('amy@one.test');
    expect(screen.queryByRole('button', { name: t('transfers.open', { address: 'amy@one.test' }) })).toBeNull();
  });
});
