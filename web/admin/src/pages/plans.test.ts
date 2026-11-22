import { cleanup, fireEvent, render, screen, within } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import App from '../App.svelte';
import type { Plan } from '../api/types';
import { App as State } from '../lib/app.svelte';
import { nav } from '../lib/nav.svelte';
import { en } from '../i18n/en';
import { setCatalogue, t } from '../i18n';
import { PLATFORM, TENANT, fakeServer, ok, signedIn } from '../test-support';

beforeEach(() => {
  setCatalogue(en, 'en');
  nav.go('/plans', true);
});
afterEach(cleanup);

describe('plans', () => {
  it('lists the plans with their limits in words and makes one from megabytes', async () => {
    const plans: Plan[] = [{ name: 'small', quota_bytes: 1_073_741_824, max_message_bytes: null, messages_per_hour: null, protocols: null }];
    const s = fakeServer(
      signedIn(PLATFORM, {
        'GET /api/v1/plans': () => ok({ items: plans }),
        'PUT /api/v1/plans': (body) => {
          const b = body as { name: string; quota_bytes: number | null; max_message_bytes: number | null };
          plans.push({ name: b.name, quota_bytes: b.quota_bytes, max_message_bytes: b.max_message_bytes, messages_per_hour: null, protocols: null });
          return ok(plans[plans.length - 1]);
        },
      }),
    );
    render(App, { app: new State(s.client) });
    const table = await screen.findByRole('table', { name: t('plans.title') });
    expect(within(table).getByText('1 GB')).toBeTruthy();
    await fireEvent.input(screen.getByLabelText(t('plans.col.name')), { target: { value: 'big' } });
    await fireEvent.input(screen.getByLabelText(t('plans.quotaMb')), { target: { value: '5120' } });
    await fireEvent.click(screen.getByRole('button', { name: t('common.save') }));
    expect(await screen.findByText(t('plans.saved', { name: 'big' }))).toBeTruthy();
    expect(s.last('PUT /api/v1/plans')?.body).toEqual({ name: 'big', quota_bytes: 5_368_709_120, max_message_bytes: null, messages_per_hour: null, protocols: null });
  });

  it('is for platform administrators: a domain administrator gets no such page', async () => {
    const s = fakeServer(signedIn(TENANT, {}));
    render(App, { app: new State(s.client) });
    expect(await screen.findByRole('heading', { level: 1 })).toBeTruthy();
    expect(s.calls.some((c) => c.key === 'PUT /api/v1/plans')).toBe(false);
    expect(screen.queryByRole('table', { name: t('plans.title') })).toBeNull();
  });
});
