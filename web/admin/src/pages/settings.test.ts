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
  nav.go('/domains/one.test/settings', true);
});
afterEach(cleanup);

const SETTINGS = { default_plan: null, max_accounts: null, quota_bytes: null, max_message_bytes: null, password_min_length: null, password_classes: null, sender_allow: ['boss@far.example'], sender_block: [], footer_text: null };

describe('domain settings: sender lists', () => {
  it('shows the lists a line each and saves what was typed as lists', async () => {
    const s = fakeServer(
      signedIn(PLATFORM, {
        'GET /api/v1/domains': ok({ items: [{ id: 1, name: 'one.test', status: 'active' }], next: null }),
        'GET /api/v1/plans': ok({ items: [] }),
        'GET /api/v1/domains/one.test/settings': ok(SETTINGS),
        'PUT /api/v1/domains/one.test/settings': ok(SETTINGS),
      }),
    );
    render(App, { app: new State(s.client) });
    const allow = (await screen.findByLabelText(t('settings.allow'))) as HTMLTextAreaElement;
    expect(allow.value).toBe('boss@far.example');
    await fireEvent.input(allow, { target: { value: 'boss@far.example\n  partner.example \n\n' } });
    await fireEvent.input(screen.getByLabelText(t('settings.block')), { target: { value: 'spam.example' } });
    await fireEvent.click(screen.getByRole('button', { name: t('common.save') }));
    expect(await screen.findByText(t('settings.saved'))).toBeTruthy();
    const body = s.last('PUT /api/v1/domains/one.test/settings')?.body as { sender_allow: string[]; sender_block: string[] };
    expect(body.sender_allow).toEqual(['boss@far.example', 'partner.example']);
    expect(body.sender_block).toEqual(['spam.example']);
  });
});
