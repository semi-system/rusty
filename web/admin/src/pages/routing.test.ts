import { cleanup, fireEvent, render, screen, within } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import App from '../App.svelte';
import type { Domain, Routing } from '../api/types';
import { App as State } from '../lib/app.svelte';
import { nav } from '../lib/nav.svelte';
import { en } from '../i18n/en';
import { setCatalogue, t } from '../i18n';
import { PLATFORM, fakeServer, ok, signedIn } from '../test-support';

beforeEach(() => {
  setCatalogue(en, 'en');
  nav.go('/domains/one.test/routing', true);
});
afterEach(cleanup);

const D: Domain = { id: 1, name: 'one.test', status: 'active' };

describe('routing', () => {
  it('lists the lists, makes one, and sets the catch-all', async () => {
    const state: Routing = { lists: [{ local: 'team', address: 'team@one.test', members: 2 }], catch_all: null, domain_aliases: ['alias.test'] };
    const s = fakeServer(
      signedIn(PLATFORM, {
        'GET /api/v1/domains': () => ok({ items: [D], next: null, total: 1 }),
        'GET /api/v1/domains/one.test/routing': () => ok(state),
        'POST /api/v1/domains/one.test/lists': (body) => {
          const local = (body as { local: string }).local;
          state.lists.push({ local, address: `${local}@one.test`, members: 0 });
          return ok({ local, address: `${local}@one.test`, members: 0 });
        },
        'PUT /api/v1/domains/one.test/catch-all': (body) => {
          state.catch_all = (body as { local: string | null }).local;
          return ok({ catch_all: state.catch_all });
        },
      }),
    );
    render(App, { app: new State(s.client) });
    const table = await screen.findByRole('table', { name: t('routing.lists') });
    expect(within(table).getByText('team@one.test')).toBeTruthy();
    expect(screen.getByText('alias.test')).toBeTruthy();
    await fireEvent.input(screen.getByLabelText(t('routing.listName')), { target: { value: 'staff' } });
    await fireEvent.click(screen.getByRole('button', { name: t('common.create') }));
    expect(await screen.findByText(t('routing.listCreated', { address: 'staff@one.test' }))).toBeTruthy();
    // The page reloads after making the list and puts the saved catch-all back in its field: type after that.
    await screen.findByRole('button', { name: t('common.create') });
    await fireEvent.input(screen.getByLabelText(t('routing.catchAllLocal')), { target: { value: 'info' } });
    const [save] = await screen.findAllByRole('button', { name: t('common.save') });
    if (save === undefined) throw new Error('no save button');
    await fireEvent.click(save);
    expect(await screen.findByText(t('routing.catchAllSet', { address: 'info@one.test' }))).toBeTruthy();
    expect(s.last('PUT /api/v1/domains/one.test/catch-all')?.body).toEqual({ local: 'info' });
  });
});
