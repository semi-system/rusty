import { cleanup, fireEvent, render, screen } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import App from '../App.svelte';
import { App as State } from '../lib/app.svelte';
import { nav } from '../lib/nav.svelte';
import { en } from '../i18n/en';
import { setCatalogue, t } from '../i18n';
import { PLATFORM, fakeServer, ok, signedIn } from '../test-support';

beforeEach(() => {
  setCatalogue(en, 'en');
  nav.go('/domains/one.test/storage', true);
});
afterEach(cleanup);

const GIB = 1_073_741_824;
const store = (status: string) => ({
  storage: {
    capacity: { declared_bytes: 10 * GIB, used_bytes: 2 * GIB, promised_bytes: 30 * GIB, unlimited_accounts: 3, status },
    kind: 's3',
    ready: true,
    check_status: 'ok',
    checked_at: 1700000000,
    settings: { endpoint: 'https://s3.example-cloud.com', region: 'eu-west-3', bucket: 'b', prefix: '', path_style: true },
    access_key_set: true,
    secret_key_set: true,
    policy: null,
    backends: null,
  },
});

describe('storage capacity', () => {
  it('says what was promised against the room declared, and saves a new room in bytes', async () => {
    const s = fakeServer(
      signedIn(PLATFORM, {
        'GET /api/v1/domains': ok({ items: [{ id: 1, name: 'one.test', status: 'active' }], next: null }),
        'GET /api/v1/domains/one.test/storage': ok(store('overcommitted')),
        'PUT /api/v1/domains/one.test/storage/capacity': ok({ queued: true }),
      }),
    );
    render(App, { app: new State(s.client) });
    expect(await screen.findByText(t('storage.capacity.overcommitted'))).toBeTruthy();
    expect(screen.getByText(t('storage.capacityUnlimited', { n: 3 }))).toBeTruthy();
    const input = screen.getByLabelText(t('storage.capacityGb')) as HTMLInputElement;
    expect(input.value).toBe('10');
    await fireEvent.input(input, { target: { value: '50' } });
    const form = input.closest('form')!;
    await fireEvent.submit(form);
    expect(await screen.findByText(t('storage.capacitySaved'))).toBeTruthy();
    expect(s.last('PUT /api/v1/domains/one.test/storage/capacity')?.body).toEqual({ capacity_bytes: 50 * GIB });
  });

  it('refuses a room that is not a positive number', async () => {
    const s = fakeServer(
      signedIn(PLATFORM, {
        'GET /api/v1/domains': ok({ items: [{ id: 1, name: 'one.test', status: 'active' }], next: null }),
        'GET /api/v1/domains/one.test/storage': ok(store('ok')),
      }),
    );
    render(App, { app: new State(s.client) });
    const input = (await screen.findByLabelText(t('storage.capacityGb'))) as HTMLInputElement;
    await fireEvent.input(input, { target: { value: '-3' } });
    await fireEvent.submit(input.closest('form')!);
    expect(await screen.findByText(t('storage.capacityInvalid'))).toBeTruthy();
    expect(s.calls.some((c) => c.key.startsWith('PUT'))).toBe(false);
  });
});
