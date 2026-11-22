import { cleanup, fireEvent, render, screen } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import App from '../App.svelte';
import type { Account, Domain } from '../api/types';
import { App as State } from '../lib/app.svelte';
import { nav } from '../lib/nav.svelte';
import { en } from '../i18n/en';
import { setCatalogue, t } from '../i18n';
import { PLATFORM, fakeServer, ok, signedIn } from '../test-support';

beforeEach(() => setCatalogue(en, 'en'));
afterEach(cleanup);

const D: Domain = { id: 1, name: 'one.test', status: 'active' };
const amy: Account = { id: 6, address: 'amy@one.test', status: 'active', has_password: true, last_login_at: null, plan: null, used_bytes: 0, quota_bytes: null, last_delivery_at: null, must_change_password: false };
const RULES = { forward: { to: ['me@other.example'], keep: true }, vacation: { enabled: false, subject: '', body: '', from: null, until: 2_000_000_000 } };

describe('mail rules', () => {
  it('shows the rules of an account and saves forwarding and the reply, keeping the end date it was given', async () => {
    nav.go('/domains/one.test/accounts', true);
    const s = fakeServer(
      signedIn(PLATFORM, {
        'GET /api/v1/domains': () => ok({ items: [D], next: null, total: 1 }),
        'GET /api/v1/domains/one.test/accounts': () => ok({ items: [amy], next: null, total: 1 }),
        'GET /api/v1/plans': () => ok({ items: [] }),
        'GET /api/v1/accounts/amy%40one.test/mail-rules': () => ok(RULES),
        'PUT /api/v1/accounts/amy%40one.test/mail-rules': () => ok(RULES),
      }),
    );
    render(App, { app: new State(s.client) });
    await fireEvent.click(await screen.findByRole('button', { name: t('rules.open', { address: 'amy@one.test' }) }));
    const forward = (await screen.findByLabelText(t('rules.forward'))) as HTMLTextAreaElement;
    expect(forward.value).toBe('me@other.example');
    await fireEvent.input(forward, { target: { value: 'me@other.example\n second@other.example ' } });
    await fireEvent.click(screen.getByLabelText(t('rules.vacation')));
    await fireEvent.input(screen.getByLabelText(t('rules.body')), { target: { value: 'Back on Monday.' } });
    await fireEvent.click(screen.getAllByRole('button', { name: t('common.save') }).at(-1)!);
    expect(await screen.findByText(t('rules.saved'))).toBeTruthy();
    expect(s.last('PUT /api/v1/accounts/amy%40one.test/mail-rules')?.body).toEqual({
      forward: { to: ['me@other.example', 'second@other.example'], keep: true },
      vacation: { enabled: true, subject: '', body: 'Back on Monday.', from: null, until: 2_000_000_000 },
    });
  });
});
