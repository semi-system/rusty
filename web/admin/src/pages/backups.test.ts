import { cleanup, render, screen, within } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import App from '../App.svelte';
import { App as State } from '../lib/app.svelte';
import { nav } from '../lib/nav.svelte';
import { en } from '../i18n/en';
import { setCatalogue, t } from '../i18n';
import { PLATFORM, fakeServer, ok, signedIn } from '../test-support';

beforeEach(() => {
  setCatalogue(en, 'en');
  nav.go('/backups', true);
});
afterEach(cleanup);

const NOW = 1_800_000_000;
const run = (id: number, ago: number, kind: string, good: boolean, bytes: number | null, detail: string) => ({ id, at: NOW - ago, kind, ok: good, bytes, detail });

describe('backups', () => {
  it('says how old the last good backup is, whether the last drill passed, and lists the runs', async () => {
    const s = fakeServer(
      signedIn(PLATFORM, {
        'GET /api/v1/backups': () => ok({ now: NOW, items: [run(3, 600, 'verify', true, null, 'migrations=39 newest=47'), run(2, 3600, 'backup', true, 5_242_880, 'database only'), run(1, 90_000, 'backup', false, null, 'pg_dump failed')] }),
      }),
    );
    render(App, { app: new State(s.client) });
    const table = await screen.findByRole('table', { name: t('backups.title') });
    expect(within(table).getAllByRole('row')).toHaveLength(4);
    expect(within(table).getByText('pg_dump failed')).toBeTruthy();
    expect(screen.getByText(/The last drill passed/)).toBeTruthy();
    expect(screen.getByText(/Last good backup/)).toBeTruthy();
    expect(screen.getByText('5.0 MiB', { selector: 'span' })).toBeTruthy();
  });

  it('warns when nothing good is on record or the last drill failed', async () => {
    const s = fakeServer(signedIn(PLATFORM, { 'GET /api/v1/backups': () => ok({ now: NOW, items: [run(1, 100, 'verify', false, null, 'could not restore')] }) }));
    render(App, { app: new State(s.client) });
    expect(await screen.findByText(t('backups.none'))).toBeTruthy();
    expect(screen.getByText(/The last drill FAILED/)).toBeTruthy();
  });

  it('shows an invitation when nothing was recorded', async () => {
    const s = fakeServer(signedIn(PLATFORM, { 'GET /api/v1/backups': () => ok({ now: NOW, items: [] }) }));
    render(App, { app: new State(s.client) });
    expect(await screen.findByText(t('backups.empty'))).toBeTruthy();
  });
});
