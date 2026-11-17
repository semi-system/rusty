import { cleanup, fireEvent, render, screen, within } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import App from '../App.svelte';
import { App as State } from '../lib/app.svelte';
import { nav } from '../lib/nav.svelte';
import { en } from '../i18n/en';
import { setCatalogue, t } from '../i18n';
import { PLATFORM, TENANT, fakeServer, ok, signedIn } from '../test-support';

beforeEach(() => {
  setCatalogue(en, 'en');
  nav.go('/protocols', true);
});
afterEach(cleanup);

describe('protocols', () => {
  it('shows POP3 as on and turns it off, then shows the answer of the server', async () => {
    let enabled = true;
    const s = fakeServer(
      signedIn(PLATFORM, {
        'GET /api/v1/protocols': () => ok({ protocols: [{ name: 'pop3', enabled }] }),
        'PUT /api/v1/protocols': () => {
          enabled = false;
          return ok({ protocols: [{ name: 'pop3', enabled }] });
        },
      }),
    );
    render(App, { app: new State(s.client) });
    const table = await screen.findByRole('table', { name: t('protocols.title') });
    expect(within(table).getByText(t('protocols.on'))).toBeTruthy();
    await fireEvent.click(within(table).getByRole('button', { name: t('protocols.turnOff') }));
    expect(await within(table).findByText(t('protocols.off'))).toBeTruthy();
    expect(s.last('PUT /api/v1/protocols')?.body).toEqual({ protocol: 'pop3', enabled: false });
  });

  it('is for platform administrators: a domain administrator gets no such page', async () => {
    const s = fakeServer(signedIn(TENANT, {}));
    render(App, { app: new State(s.client) });
    expect(await screen.findByRole('heading', { level: 1 })).toBeTruthy();
    expect(screen.queryByRole('table', { name: t('protocols.title') })).toBeNull();
    expect(s.calls.some((c) => c.key === 'GET /api/v1/protocols')).toBe(false);
  });
});
