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
  nav.go('/domains/one.test/dkim', true);
});
afterEach(cleanup);

const key = (selector: string) => ({ selector, algorithm: 'ed25519', record_name: `${selector}._domainkey.one.test`, record_value: 'v=DKIM1; k=ed25519; p=AAAA', sealed: true, created_at: 1700000000 });
const DOMAIN = { 'GET /api/v1/domains': ok({ items: [{ id: 1, name: 'one.test', status: 'active' }], next: null }) };

describe('DKIM rotation', () => {
  it('retires the old key after a confirmation, and not when it is the only one', async () => {
    let items = [key('old-ed25519'), key('r20261122-ed25519')];
    const s = fakeServer(
      signedIn(PLATFORM, {
        ...DOMAIN,
        'GET /api/v1/domains/one.test/dkim': () => ok({ items }),
        'DELETE /api/v1/domains/one.test/dkim/old-ed25519': () => {
          items = [key('r20261122-ed25519')];
          return ok({});
        },
      }),
    );
    render(App, { app: new State(s.client) });
    await fireEvent.click(await screen.findByRole('button', { name: t('dkim.retireFor', { selector: 'old-ed25519' }) }));
    await fireEvent.click(screen.getByRole('button', { name: t('dkim.retireYes') }));
    expect(await screen.findByText(t('dkim.retired', { selector: 'old-ed25519' }))).toBeTruthy();
    expect(s.calls.some((c) => c.key === 'DELETE /api/v1/domains/one.test/dkim/old-ed25519')).toBe(true);
    expect(screen.queryByRole('button', { name: t('dkim.retire') })).toBeNull();
  });

  it('rotates: makes a key under a selector named after the day', async () => {
    const s = fakeServer(
      signedIn(PLATFORM, {
        ...DOMAIN,
        'GET /api/v1/domains/one.test/dkim': () => ok({ items: [key('rusty-ed25519')] }),
        'POST /api/v1/domains/one.test/dkim': () => ok(key('r20261122-ed25519')),
      }),
    );
    render(App, { app: new State(s.client) });
    await fireEvent.click(await screen.findByRole('button', { name: t('dkim.rotate') }));
    await screen.findByText(t('dkim.created', { selector: 'r20261122-ed25519' }));
    const body = s.last('POST /api/v1/domains/one.test/dkim')?.body as { selector: string };
    expect(body.selector).toMatch(/^r\d{8}$/);
  });
});
