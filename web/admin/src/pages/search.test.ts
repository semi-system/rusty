import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import App from '../App.svelte';
import { App as State } from '../lib/app.svelte';
import { nav } from '../lib/nav.svelte';
import { en } from '../i18n/en';
import { setCatalogue, t } from '../i18n';
import { PLATFORM, created, fail, fakeServer, ok, signedIn } from '../test-support';

beforeEach(() => {
  setCatalogue(en, 'en');
  nav.go('/search', true);
});
afterEach(cleanup);

const found = (id: number, state: string, results: unknown[], truncated = false, error: string | null = null) => ({ id, state, found: results.length, truncated, results, error, created_at: 1, finished_at: state === 'done' ? 2 : null });
const row = { account: 'alice@one.test', folder: 'Inbox', at: 1_700_000_000, from: 'Bob <bob@else.test>', subject: 'Invoice 42', size: 2048 };

describe('search', () => {
  it('asks for what was typed, waits for the worker and lists what it found', async () => {
    const s = fakeServer(
      signedIn(PLATFORM, {
        'POST /api/v1/search': () => created({ id: 7, state: 'queued' }),
        'GET /api/v1/search/7': () => ok(found(7, 'done', [row])),
      }),
    );
    render(App, { app: new State(s.client) });
    await fireEvent.input(await screen.findByLabelText(t('search.subject')), { target: { value: ' invoice ' } });
    await fireEvent.input(screen.getByLabelText(t('search.since')), { target: { value: '2026-01-31' } });
    await fireEvent.click(screen.getByRole('button', { name: t('search.go') }));
    const table = await screen.findByRole('table', { name: t('search.title') });
    expect(within(table).getByText('Invoice 42')).toBeTruthy();
    expect(within(table).getByText('alice@one.test')).toBeTruthy();
    expect(s.last('POST /api/v1/search')?.body).toEqual({ subject: 'invoice', since: 1769817600 });
  });

  it('refuses to ask for nothing, and says when the search is off', async () => {
    const s = fakeServer(signedIn(PLATFORM, { 'POST /api/v1/search': () => fail(409, 'wrong_state') }));
    render(App, { app: new State(s.client) });
    await fireEvent.click(await screen.findByRole('button', { name: t('search.go') }));
    expect(await screen.findByText(t('search.invalid'))).toBeTruthy();
    expect(s.last('POST /api/v1/search')).toBeUndefined();
    await fireEvent.input(screen.getByLabelText(t('search.from')), { target: { value: 'bob' } });
    await fireEvent.click(screen.getByRole('button', { name: t('search.go') }));
    await waitFor(() => expect(screen.getByText(t('search.off'))).toBeTruthy());
  });
});
