import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import App from '../App.svelte';
import type { Domain, QueueJob } from '../api/types';
import { App as State } from '../lib/app.svelte';
import { nav } from '../lib/nav.svelte';
import { en } from '../i18n/en';
import { setCatalogue, t } from '../i18n';
import { PLATFORM, fakeServer, ok, signedIn } from '../test-support';

beforeEach(() => {
  setCatalogue(en, 'en');
  nav.go('/domains/one.test/queue', true);
});
afterEach(cleanup);

const D: Domain = { id: 1, name: 'one.test', status: 'active' };
const job = (id: number, state: string, over: Partial<QueueJob> = {}): QueueJob => ({
  id,
  state,
  sender: 'alice@one.test',
  recipients: [`user${id}@far.example`],
  destination: 'far.example',
  attempts: 1,
  created_at: 1_700_000_000,
  next_attempt_at: state === 'waiting' ? 1_700_000_600 : null,
  last_error: state === 'waiting' ? 'greylisted' : null,
  ...over,
});

describe('queue', () => {
  it('offers the actions that fit each state, sends a waiting message now, and asks before deleting', async () => {
    let jobs = [job(3, 'dead'), job(2, 'held'), job(1, 'waiting')];
    const s = fakeServer(
      signedIn(PLATFORM, {
        'GET /api/v1/domains': () => ok({ items: [D], next: null, total: 1 }),
        'GET /api/v1/domains/one.test/queue': () => ok({ items: jobs, next: null }),
        'GET /api/v1/domains/one.test/trace': () => ok({ items: [{ id: 1, direction: 'out', at: 1_700_000_100, sender: 'alice@one.test', recipient: 'user1@far.example', event: 'deferred', remote: 'mx.far.example', status: null, detail: 'greylisted' }], next: null }),
        'POST /api/v1/domains/one.test/queue/1/release': () => {
          jobs = jobs.filter((j) => j.id !== 1);
          return ok({});
        },
      }),
    );
    render(App, { app: new State(s.client) });
    expect(await screen.findByRole('button', { name: t('queue.sendNowFor', { id: 1 }) })).toBeTruthy();
    expect(screen.getByRole('button', { name: t('queue.releaseFor', { id: 2 }) }), 'a held message is released').toBeTruthy();
    expect(screen.getByRole('button', { name: t('queue.retryFor', { id: 3 }) }), 'a given-up one is tried again').toBeTruthy();
    expect(screen.queryByRole('button', { name: t('queue.retryFor', { id: 1 }) })).toBeNull();
    expect(await screen.findByText('mx.far.example · greylisted')).toBeTruthy();
    await fireEvent.click(screen.getByRole('button', { name: t('queue.sendNowFor', { id: 1 }) }));
    await screen.findByRole('button', { name: t('queue.releaseFor', { id: 2 }) });
    expect(s.calls.some((c) => c.key === 'POST /api/v1/domains/one.test/queue/1/release')).toBe(true);
    // Deleting asks first.
    // The page is busy until the action has been answered: wait for its buttons to come back.
    await waitFor(() => expect((screen.getByRole('button', { name: t('queue.deleteFor', { id: 2 }) }) as HTMLButtonElement).disabled).toBe(false));
    await fireEvent.click(screen.getByRole('button', { name: t('queue.deleteFor', { id: 2 }) }));
    expect(s.calls.some((c) => c.key.startsWith('DELETE'))).toBe(false);
    expect(await screen.findByRole('button', { name: t('queue.deleteYes') })).toBeTruthy();
  });
});
