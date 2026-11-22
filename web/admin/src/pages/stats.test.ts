import { cleanup, fireEvent, render, screen, within } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import App from '../App.svelte';
import { App as State } from '../lib/app.svelte';
import { nav } from '../lib/nav.svelte';
import { en } from '../i18n/en';
import { setCatalogue, t } from '../i18n';
import { PLATFORM, fakeServer, ok, signedIn } from '../test-support';

beforeEach(() => {
  setCatalogue(en, 'en');
  nav.go('/domains/one.test/stats', true);
});
afterEach(cleanup);

describe('statistics', () => {
  it('shows the counts of each day', async () => {
    const s = fakeServer(
      signedIn(PLATFORM, {
        'GET /api/v1/domains': ok({ items: [{ id: 1, name: 'one.test', status: 'active' }], next: null }),
        'GET /api/v1/domains/one.test/stats': ok({ days: [{ day: 1700006400, received: 12, rejected: 3, sent: 7, deferred: 2, failed: 1 }] }),
      }),
    );
    render(App, { app: new State(s.client) });
    const table = await screen.findByRole('table', { name: t('stats.list') });
    const row = within(table).getAllByRole('row')[1]!;
    expect(within(row).getAllByRole('cell').map((c) => c.textContent)).toEqual(['12', '3', '7', '2', '1']);
  });
});

describe('statistics range', () => {
  it('asks for a longer period when it is chosen', async () => {
    const s = fakeServer(
      signedIn(PLATFORM, {
        'GET /api/v1/domains': ok({ items: [{ id: 1, name: 'one.test', status: 'active' }], next: null }),
        'GET /api/v1/domains/one.test/stats': ok({ days: [] }),
        'GET /api/v1/domains/one.test/stats?days=365': ok({ days: [{ day: 1700006400, received: 1, rejected: 0, sent: 2, deferred: 0, failed: 0 }] }),
      }),
    );
    render(App, { app: new State(s.client) });
    const select = (await screen.findByLabelText(t('stats.range'))) as HTMLSelectElement;
    await fireEvent.change(select, { target: { value: '365' } });
    expect(await screen.findByRole('table', { name: t('stats.list') })).toBeTruthy();
    expect(s.calls.some((c) => c.key === 'GET /api/v1/domains/one.test/stats?days=365')).toBe(true);
  });
});
