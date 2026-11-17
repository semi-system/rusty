import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import App from '../App.svelte';
import { ApiError, Client, type Fetch } from '../api/client';
import type { Me } from '../api/types';
import { App as State } from '../lib/app.svelte';
import { en } from '../i18n/en';
import { pseudo, setCatalogue, t } from '../i18n';

const ME: Me = { address: 'root@example.org', scope: 'platform', domain: null, csrf: 'c'.repeat(64) };

type Answer = { status: number; body?: unknown; headers?: Record<string, string> };

/** A server that answers by "METHOD path", every call recorded. */
function server(routes: Record<string, Answer | Answer[]>): { client: Client; calls: string[]; bodies: unknown[] } {
  const calls: string[] = [];
  const bodies: unknown[] = [];
  const fetcher: Fetch = async (path, init) => {
    const key = `${init?.method ?? 'GET'} ${path}`;
    calls.push(key);
    bodies.push(init?.body === undefined ? undefined : JSON.parse(String(init.body)));
    const r = routes[key];
    const a = Array.isArray(r) ? r.shift() : r;
    if (a === undefined && key === 'GET /api/v1/domains') {
      // The signed-in shell lists the domains: an empty list unless a test says otherwise.
      return new Response(JSON.stringify({ items: [], next: null }), { status: 200 });
    }
    if (a === undefined) {
      return new Response(JSON.stringify({ error: 'not_found' }), { status: 404 });
    }
    return new Response(a.body === undefined ? '' : JSON.stringify(a.body), { status: a.status, headers: a.headers });
  };
  return { client: new Client(fetcher), calls, bodies };
}

beforeEach(() => setCatalogue(en, 'en'));
afterEach(cleanup);

describe('boot', () => {
  it('shows the application when a session exists', async () => {
    const s = server({ 'GET /api/v1/session': { status: 200, body: ME } });
    render(App, { app: new State(s.client) });
    expect(await screen.findByText(ME.address)).toBeTruthy();
    expect(screen.getByText(t('home.scopePlatform'), { exact: false })).toBeTruthy();
    expect(s.calls).toEqual(['GET /api/v1/session', 'GET /api/v1/domains']);
  });

  it('shows sign-in when nobody is signed in and setup is closed', async () => {
    const s = server({ 'GET /api/v1/session': { status: 401, body: { error: 'unauthorized' } }, 'GET /api/v1/setup': { status: 404, body: { error: 'not_found' } } });
    render(App, { app: new State(s.client) });
    expect(await screen.findByRole('heading', { name: t('login.title') })).toBeTruthy();
  });

  it('shows the setup page while it is open', async () => {
    const s = server({ 'GET /api/v1/session': { status: 401, body: { error: 'unauthorized' } }, 'GET /api/v1/setup': { status: 200, body: { required: true } } });
    render(App, { app: new State(s.client) });
    expect(await screen.findByRole('heading', { name: t('setup.title') })).toBeTruthy();
  });

  it('says plainly that the server cannot be reached', async () => {
    const fetcher: Fetch = async () => {
      throw new TypeError('x');
    };
    render(App, { app: new State(new Client(fetcher)) });
    expect((await screen.findByRole('alert')).textContent).toBe(t('error.network'));
  });
});

describe('sign-in', () => {
  async function loginPage(extra: Record<string, Answer | Answer[]> = {}) {
    const s = server({ 'GET /api/v1/session': [{ status: 401, body: { error: 'unauthorized' } }], 'GET /api/v1/setup': { status: 404, body: { error: 'not_found' } }, ...extra });
    render(App, { app: new State(s.client) });
    await screen.findByRole('heading', { name: t('login.title') });
    return s;
  }

  it('moves focus to the first field and labels both fields', async () => {
    await loginPage();
    const address = screen.getByLabelText(t('login.address')) as HTMLInputElement;
    expect(document.activeElement).toBe(address);
    expect(address.autocomplete).toBe('username');
    expect((screen.getByLabelText(t('login.password')) as HTMLInputElement).autocomplete).toBe('current-password');
  });

  it('signs in and shows the application', async () => {
    const s = await loginPage({ 'POST /api/v1/session': { status: 200, body: ME } });
    await fireEvent.input(screen.getByLabelText(t('login.address')), { target: { value: 'root@example.org' } });
    await fireEvent.input(screen.getByLabelText(t('login.password')), { target: { value: 'pw' } });
    await fireEvent.click(screen.getByRole('button', { name: t('login.submit') }));
    expect(await screen.findByText(ME.address)).toBeTruthy();
    expect(s.bodies.find((b) => b !== undefined)).toEqual({ address: 'root@example.org', password: 'pw' });
  });

  it('shows one fixed message for a refusal and clears the password field', async () => {
    await loginPage({ 'POST /api/v1/session': { status: 401, body: { error: 'unauthorized' } } });
    const pw = screen.getByLabelText(t('login.password')) as HTMLInputElement;
    await fireEvent.input(screen.getByLabelText(t('login.address')), { target: { value: 'a@b.c' } });
    await fireEvent.input(pw, { target: { value: 'wrong' } });
    await fireEvent.click(screen.getByRole('button', { name: t('login.submit') }));
    expect((await screen.findByRole('alert')).textContent).toBe(t('error.unauthorized'));
    expect(pw.value).toBe('');
  });

  it('shows the delay of a throttled sign-in', async () => {
    await loginPage({ 'POST /api/v1/session': { status: 429, body: { error: 'throttled' }, headers: { 'retry-after': '120' } } });
    await fireEvent.input(screen.getByLabelText(t('login.address')), { target: { value: 'a@b.c' } });
    await fireEvent.input(screen.getByLabelText(t('login.password')), { target: { value: 'x' } });
    await fireEvent.click(screen.getByRole('button', { name: t('login.submit') }));
    expect((await screen.findByRole('alert')).textContent).toBe(t('error.throttled', { seconds: 120 }));
  });

  it('signs out and returns to sign-in even when the server fails', async () => {
    const s = server({ 'GET /api/v1/session': { status: 200, body: ME }, 'DELETE /api/v1/session': { status: 503, body: { error: 'busy' } } });
    const app = new State(s.client);
    render(App, { app });
    await fireEvent.click(await screen.findByRole('button', { name: t('nav.signOut') }));
    await waitFor(() => expect(app.view.kind).toBe('signin'));
  });
});

describe('setup', () => {
  async function setupPage(extra: Record<string, Answer | Answer[]> = {}) {
    const s = server({ 'GET /api/v1/session': [{ status: 401, body: { error: 'unauthorized' } }, { status: 200, body: ME }], 'GET /api/v1/setup': { status: 200, body: { required: true } }, ...extra });
    render(App, { app: new State(s.client) });
    await screen.findByRole('heading', { name: t('setup.title') });
    return s;
  }
  const fill = async (token: string, address: string, password: string, confirm: string) => {
    await fireEvent.input(screen.getByLabelText(t('setup.token')), { target: { value: token } });
    await fireEvent.input(screen.getByLabelText(t('setup.address')), { target: { value: address } });
    await fireEvent.input(screen.getByLabelText(t('setup.password', { min: 12 })), { target: { value: password } });
    await fireEvent.input(screen.getByLabelText(t('setup.confirm')), { target: { value: confirm } });
    await fireEvent.click(screen.getByRole('button', { name: t('setup.submit') }));
  };

  it('does not call the server for a short password or two different ones', async () => {
    const s = await setupPage();
    await fill('tok', 'root@example.org', 'short', 'short');
    expect((await screen.findByRole('alert')).textContent).toBe(t('setup.tooShort', { min: 12 }));
    await fill('tok', 'root@example.org', 'a-long-password-1', 'a-long-password-2');
    await waitFor(() => expect(screen.getByRole('alert').textContent).toBe(t('setup.mismatch')));
    expect(s.calls.filter((c) => c.startsWith('POST'))).toEqual([]);
  });

  it('keeps the token in a password field and sends it trimmed', async () => {
    const s = await setupPage({ 'POST /api/v1/setup': { status: 201, body: ME } });
    expect((screen.getByLabelText(t('setup.token')) as HTMLInputElement).type).toBe('password');
    await fill('  tok123  ', 'root@example.org', 'a-long-password-1', 'a-long-password-1');
    expect(await screen.findByText(ME.address)).toBeTruthy();
    expect(s.bodies.find((b) => typeof b === 'object' && b !== null && 'token' in b)).toEqual({ token: 'tok123', address: 'root@example.org', password: 'a-long-password-1' });
  });

  it('says so when the token is wrong', async () => {
    await setupPage({ 'POST /api/v1/setup': { status: 401, body: { error: 'unauthorized' } } });
    await fill('nope', 'root@example.org', 'a-long-password-1', 'a-long-password-1');
    expect((await screen.findByRole('alert')).textContent).toBe(t('error.unauthorized'));
  });
});

describe('the message catalogue', () => {
  it('has no text of its own in a page: with the pseudo-locale every visible string is wrapped', async () => {
    setCatalogue(pseudo(en), 'xx');
    const s = server({ 'GET /api/v1/session': { status: 401, body: { error: 'unauthorized' } }, 'GET /api/v1/setup': { status: 404, body: { error: 'not_found' } } });
    const { container } = render(App, { app: new State(s.client) });
    await screen.findByRole('heading');
    expect(document.documentElement.lang).toBe('xx');
    const texts: string[] = [];
    const walker = document.createTreeWalker(container, NodeFilter.SHOW_TEXT);
    for (let n = walker.nextNode(); n !== null; n = walker.nextNode()) {
      const v = (n.textContent ?? '').trim();
      if (v !== '') texts.push(v);
    }
    expect(texts.length).toBeGreaterThan(3);
    for (const v of texts) {
      expect(v.startsWith('[') && v.endsWith('~~~]'), `a string that is not from the catalogue: ${v}`).toBe(true);
    }
    // Accessible names and labels as well.
    for (const el of container.querySelectorAll('[aria-label], [title], [placeholder], [alt]')) {
      for (const a of ['aria-label', 'title', 'placeholder', 'alt']) {
        const v = el.getAttribute(a);
        if (v !== null) expect(v.startsWith('[')).toBe(true);
      }
    }
  });

  it('fills placeholders and leaves a missing one visible', () => {
    expect(t('setup.tooShort', { min: 12 })).toBe('The password must have at least 12 characters.');
    expect(t('setup.tooShort')).toContain('{min}');
    expect(new ApiError(1, 'x').code).toBe('x');
  });
});
