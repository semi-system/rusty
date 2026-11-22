import { cleanup, render, screen, within } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import App from '../App.svelte';
import { App as State } from '../lib/app.svelte';
import { nav } from '../lib/nav.svelte';
import { en } from '../i18n/en';
import { setCatalogue, t } from '../i18n';
import { PLATFORM, TENANT, fakeServer, ok, signedIn } from '../test-support';

beforeEach(() => {
  setCatalogue(en, 'en');
  nav.go('/certificates', true);
});
afterEach(cleanup);

describe('certificates', () => {
  it('shows what each public name presents and why a probe failed', async () => {
    const s = fakeServer(
      signedIn(PLATFORM, {
        'GET /api/v1/certificates': ok({
          items: [
            { host: 'mail.one.test', port: 993, purpose: 'imap', subject: 'mail.one.test', issuer: 'R11', not_before: 1700000000, not_after: 1710000000, days_left: 9, error: null },
            { host: 'mail.one.test', port: 995, purpose: 'pop3', subject: null, issuer: null, not_before: null, not_after: null, days_left: null, error: 'cannot connect: refused' },
          ],
        }),
      }),
    );
    render(App, { app: new State(s.client) });
    const table = await screen.findByRole('table', { name: t('certs.title') });
    expect(within(table).getByText(t('certs.days', { n: 9 }))).toBeTruthy();
    expect(within(table).getByText('cannot connect: refused')).toBeTruthy();
  });

  it('is for platform administrators: a domain administrator gets no such page', async () => {
    const s = fakeServer(signedIn(TENANT, {}));
    render(App, { app: new State(s.client) });
    expect(await screen.findByRole('heading', { level: 1 })).toBeTruthy();
    expect(s.calls.some((c) => c.key === 'GET /api/v1/certificates')).toBe(false);
  });
});
