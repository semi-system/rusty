import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import App from '../App.svelte';
import { App as State } from '../lib/app.svelte';
import { nav } from '../lib/nav.svelte';
import { en } from '../i18n/en';
import { setCatalogue, t } from '../i18n';
import { PLATFORM, fakeServer, ok, signedIn } from '../test-support';

beforeEach(() => {
  setCatalogue(en, 'en');
  nav.go('/audit', true);
});
afterEach(cleanup);

const entry = (id: number, path: string) => ({ id, at: 1700000000 + id, actor: 'root@home.test', domain_id: null, ip: '203.0.113.9', method: 'POST', route: path, path, status: 201, detail: id === 3 ? '{"virus_action":"reject","password":"(hidden)"}' : null });

describe('audit', () => {
  it('lists the changes newest first, pages on and filters by domain', async () => {
    const s = fakeServer(
      signedIn(PLATFORM, {
        'GET /api/v1/audit': () => ok({ items: [entry(3, '/api/v1/domains'), entry(2, '/api/v1/plans')], next: 2 }),
        'GET /api/v1/audit?before=2': () => ok({ items: [entry(1, '/api/v1/protocols')], next: null }),
        'GET /api/v1/audit?domain=one.test': () => ok({ items: [entry(2, '/api/v1/plans')], next: null }),
      }),
    );
    render(App, { app: new State(s.client) });
    const table = await screen.findByRole('table', { name: t('audit.title') });
    expect(within(table).getByText('POST /api/v1/domains')).toBeTruthy();
    expect(within(table).getByText('virus_action')).toBeTruthy();
    expect(within(table).getByText('reject')).toBeTruthy();
    expect(within(table).getByText(t('audit.hidden'))).toBeTruthy();
    await fireEvent.click(screen.getByRole('button', { name: t('audit.more') }));
    expect(await within(table).findByText('POST /api/v1/protocols')).toBeTruthy();
    expect(screen.queryByRole('button', { name: t('audit.more') })).toBeNull();
    await fireEvent.input(screen.getByLabelText(t('audit.domain')), { target: { value: 'one.test' } });
    await fireEvent.click(screen.getByRole('button', { name: t('audit.find') }));
    await waitFor(() => expect(within(screen.getByRole('table', { name: t('audit.title') })).queryByText('POST /api/v1/domains')).toBeNull());
    expect(s.last('GET /api/v1/audit?domain=one.test')).toBeTruthy();
  });
});
