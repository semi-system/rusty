import { cleanup, fireEvent, render, screen } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import App from '../App.svelte';
import { App as State } from '../lib/app.svelte';
import { nav } from '../lib/nav.svelte';
import { en } from '../i18n/en';
import { setCatalogue, t } from '../i18n';
import { PLATFORM, TENANT, fakeServer, ok, signedIn } from '../test-support';

beforeEach(() => {
  setCatalogue(en, 'en');
  nav.go('/platform', true);
});
afterEach(cleanup);

describe('platform settings', () => {
  it('shows the limits and saves the changed ones, sizes in megabytes', async () => {
    const s = fakeServer(
      signedIn(PLATFORM, {
        'GET /api/v1/settings': () => ok({ quota_bytes: 1_073_741_824, max_message_bytes: null, messages_per_hour: null }),
        'PUT /api/v1/settings': () => ok({ quota_bytes: 2_097_152, max_message_bytes: 26_214_400, messages_per_hour: 50 }),
      }),
    );
    render(App, { app: new State(s.client) });
    const quota = (await screen.findByLabelText(t('platform.quotaMb'))) as HTMLInputElement;
    expect(quota.value).toBe('1024');
    await fireEvent.input(quota, { target: { value: '2' } });
    await fireEvent.input(screen.getByLabelText(t('platform.maxMessageMb')), { target: { value: '25' } });
    await fireEvent.input(screen.getByLabelText(t('platform.perHour')), { target: { value: '50' } });
    await fireEvent.click(screen.getByRole('button', { name: t('common.save') }));
    expect(await screen.findByText(t('platform.saved'))).toBeTruthy();
    expect(s.last('PUT /api/v1/settings')?.body).toEqual({ quota_bytes: 2_097_152, max_message_bytes: 26_214_400, messages_per_hour: 50 });
  });

  it('is for platform administrators: a domain administrator gets no such page', async () => {
    const s = fakeServer(signedIn(TENANT, {}));
    render(App, { app: new State(s.client) });
    expect(await screen.findByRole('heading', { level: 1 })).toBeTruthy();
    expect(s.calls.some((c) => c.key === 'GET /api/v1/settings')).toBe(false);
  });
});
