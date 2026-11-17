import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import App from '../App.svelte';
import type { Account, Alias, Dkim, DnsRecord, Domain, Me, Relay, Storage } from '../api/types';
import { App as State } from '../lib/app.svelte';
import { nav } from '../lib/nav.svelte';
import { en } from '../i18n/en';
import { setCatalogue, t } from '../i18n';
import { PLATFORM, TENANT, created, fail, fakeServer, ok, signedIn, type Answer, type Handler } from '../test-support';

const D1: Domain = { id: 1, name: 'example.org', status: 'active' };
const D2: Domain = { id: 2, name: 'other.org', status: 'disabled' };
const ACC: Account = { id: 10, address: 'alice@example.org', status: 'active', has_password: true };
const ACC2: Account = { id: 11, address: 'bob@example.org', status: 'disabled', has_password: false };

beforeEach(() => {
  setCatalogue(en, 'en');
  nav.go('/', true);
});
afterEach(cleanup);

async function open(path: string, me: Me, routes: Record<string, Handler>) {
  nav.go(path, true);
  const s = fakeServer(signedIn(me, routes));
  render(App, { app: new State(s.client) });
  return s;
}

const field = (label: string): HTMLInputElement => screen.getByLabelText(label) as HTMLInputElement;
const type = async (label: string, value: string) => {
  // The page appears after the session and the first data have arrived: wait for the field.
  await screen.findByLabelText(label);
  await fireEvent.input(field(label), { target: { value } });
};

describe('domains', () => {
  it('lists the domains with their status and links each to its overview', async () => {
    await open('/', PLATFORM, { 'GET /api/v1/domains': ok({ items: [D1, D2], next: null }) });
    const table = await screen.findByRole('table', { name: t('domains.list') });
    expect(within(table).getByRole('link', { name: 'example.org' }).getAttribute('href')).toBe('/domains/example.org/overview');
    expect(within(table).getByText(t('domain.status.disabled'))).toBeTruthy();
  });

  it('shows more pages by cursor', async () => {
    const s = await open('/', PLATFORM, {
      'GET /api/v1/domains': ok({ items: [D1], next: 1 }),
      'GET /api/v1/domains?after=1': ok({ items: [D2], next: null }),
    });
    await fireEvent.click(await screen.findByRole('button', { name: t('common.loadMore') }));
    expect(await screen.findByRole('link', { name: 'other.org' })).toBeTruthy();
    expect(screen.getByRole('link', { name: 'example.org' })).toBeTruthy();
    expect(screen.queryByRole('button', { name: t('common.loadMore') })).toBeNull();
    expect(s.calls.map((c) => c.key)).toContain('GET /api/v1/domains?after=1');
  });

  it('lets a platform administrator create a domain, with the CSRF token', async () => {
    const list: Answer[] = [ok({ items: [], next: null }), ok({ items: [D1], next: null })];
    const s = await open('/', PLATFORM, { 'GET /api/v1/domains': list, 'POST /api/v1/domains': created(D1) });
    await type(t('domains.newName'), 'example.org');
    await fireEvent.click(screen.getByRole('button', { name: t('common.create') }));
    expect(await screen.findByText(t('domains.created', { name: 'example.org' }))).toBeTruthy();
    const post = s.last('POST /api/v1/domains')!;
    expect(post.body).toEqual({ name: 'example.org' });
    expect(post.headers['X-CSRF-Token']).toBe(PLATFORM.csrf);
    expect(await screen.findByRole('link', { name: 'example.org' })).toBeTruthy();
  });

  it('does not offer a domain administrator the creation of domains', async () => {
    await open('/', TENANT, { 'GET /api/v1/domains': ok({ items: [D1], next: null }) });
    await screen.findByRole('link', { name: 'example.org' });
    expect(screen.queryByText(t('domains.new'))).toBeNull();
  });

  it('shows the fixed message of a refusal', async () => {
    await open('/', PLATFORM, { 'GET /api/v1/domains': ok({ items: [], next: null }), 'POST /api/v1/domains': fail(409, 'exists') });
    await type(t('domains.newName'), 'example.org');
    await fireEvent.click(await screen.findByRole('button', { name: t('common.create') }));
    expect((await screen.findByRole('alert')).textContent).toBe(t('error.exists'));
  });
});

describe('overview', () => {
  const routes = { 'GET /api/v1/domains': ok({ items: [D1, D2], next: null }), 'GET /api/v1/domains/example.org/outbound': ok({ queued: 3, running: 1, failed: 0, dead: 2 }) };

  it('shows the status, the tabs and the queue figures', async () => {
    await open('/domains/example.org/overview', PLATFORM, routes);
    expect(await screen.findByText(t('domain.queueQueued', { n: 3 }))).toBeTruthy();
    expect(screen.getByText(t('domain.queueDead', { n: 2 }))).toBeTruthy();
    const tabs = screen.getByRole('navigation', { name: t('domain.tabs', { name: 'example.org' }) });
    expect(within(tabs).getAllByRole('link').map((a) => a.getAttribute('href'))).toEqual(
      ['overview', 'accounts', 'aliases', 'dkim', 'dns', 'relay', 'storage'].map((p) => `/domains/example.org/${p}`),
    );
    expect(within(tabs).getByRole('link', { name: t('domain.overview') }).getAttribute('aria-current')).toBe('page');
  });

  it('lets a platform administrator disable the domain and warns that it is off', async () => {
    const s = await open('/domains/example.org/overview', PLATFORM, { ...routes, 'POST /api/v1/domains/example.org/disable': ok({ ...D1, status: 'disabled' }) });
    await fireEvent.click(await screen.findByRole('button', { name: t('domain.disable') }));
    expect(await screen.findByText(t('domain.disabledNote'))).toBeTruthy();
    expect(screen.getByRole('button', { name: t('domain.enable') })).toBeTruthy();
    expect(s.last('POST /api/v1/domains/example.org/disable')!.headers['X-CSRF-Token']).toBe(PLATFORM.csrf);
  });

  it('does not offer a domain administrator the status button', async () => {
    await open('/domains/example.org/overview', TENANT, routes);
    await screen.findByText(t('domain.queueQueued', { n: 3 }));
    expect(screen.queryByRole('button', { name: t('domain.disable') })).toBeNull();
  });

  it('says not found for a domain that is not in the list (another tenant looks the same)', async () => {
    await open('/domains/nowhere.org/overview', TENANT, { 'GET /api/v1/domains': ok({ items: [D1], next: null }) });
    expect((await screen.findByRole('alert')).textContent).toBe(t('error.not_found'));
  });
});

describe('accounts', () => {
  const list = ok({ items: [ACC, ACC2], next: null });

  it('lists accounts, creates one with its role addresses and never shows a password', async () => {
    const s = await open('/domains/example.org/accounts', PLATFORM, {
      'GET /api/v1/domains/example.org/accounts': [list, ok({ items: [ACC, ACC2], next: null })],
      'POST /api/v1/domains/example.org/accounts': created({ id: 12, address: 'carol@example.org', status: 'active', has_password: true, aliases_created: ['postmaster@example.org', 'abuse@example.org'] }),
    });
    expect(await screen.findByText('alice@example.org')).toBeTruthy();
    await type(t('accounts.local'), 'carol');
    await type(t('accounts.newPassword', { min: 8 }), 'carol-password-1');
    await fireEvent.click(screen.getByRole('button', { name: t('common.create') }));
    const note = await screen.findByText(new RegExp('carol@example\\.org was created'));
    expect(note.textContent).toContain('postmaster@example.org, abuse@example.org');
    expect(s.last('POST /api/v1/domains/example.org/accounts')!.body).toEqual({ local: 'carol', password: 'carol-password-1' });
    expect(document.body.textContent).not.toContain('carol-password-1');
    expect(field(t('accounts.newPassword', { min: 8 })).value).toBe('');
  });

  it('refuses a short password without asking the server, and an empty one means none', async () => {
    const s = await open('/domains/example.org/accounts', PLATFORM, { 'GET /api/v1/domains/example.org/accounts': list, 'POST /api/v1/domains/example.org/accounts': created({ id: 13, address: 'dave@example.org', status: 'active', has_password: false, aliases_created: [] }) });
    await screen.findByText('alice@example.org');
    await type(t('accounts.local'), 'dave');
    await type(t('accounts.newPassword', { min: 8 }), 'short');
    await fireEvent.click(screen.getByRole('button', { name: t('common.create') }));
    expect((await screen.findByRole('alert')).textContent).toBe(t('accounts.tooShort', { min: 8 }));
    expect(s.calls.some((c) => c.key.startsWith('POST'))).toBe(false);
    await type(t('accounts.newPassword', { min: 8 }), '');
    await fireEvent.click(screen.getByRole('button', { name: t('common.create') }));
    await waitFor(() => expect(s.last('POST /api/v1/domains/example.org/accounts')!.body).toEqual({ local: 'dave' }));
  });

  it('disables and enables an account in place', async () => {
    const s = await open('/domains/example.org/accounts', PLATFORM, {
      'GET /api/v1/domains/example.org/accounts': list,
      'POST /api/v1/accounts/alice%40example.org/disable': ok({ ...ACC, status: 'disabled' }),
    });
    await fireEvent.click(await screen.findByRole('button', { name: t('accounts.disable', { address: 'alice@example.org' }) }));
    expect(await screen.findByRole('button', { name: t('accounts.enable', { address: 'alice@example.org' }) })).toBeTruthy();
    expect(s.calls.some((c) => c.key === 'POST /api/v1/accounts/alice%40example.org/disable')).toBe(true);
  });

  it('sets a password through a form that is closed and cleared afterwards', async () => {
    const s = await open('/domains/example.org/accounts', PLATFORM, {
      'GET /api/v1/domains/example.org/accounts': list,
      'POST /api/v1/accounts/alice%40example.org/password': ok(),
    });
    await fireEvent.click(await screen.findByRole('button', { name: t('accounts.setPassword', { address: 'alice@example.org' }) }));
    await type(t('accounts.passwordFor', { address: 'alice@example.org' }), 'a-new-password-1');
    await fireEvent.click(screen.getByRole('button', { name: t('common.save') }));
    expect(await screen.findByText(t('accounts.passwordSet', { address: 'alice@example.org' }))).toBeTruthy();
    expect(s.last('POST /api/v1/accounts/alice%40example.org/password')!.body).toEqual({ password: 'a-new-password-1' });
    expect(screen.queryByLabelText(t('accounts.passwordFor', { address: 'alice@example.org' }))).toBeNull();
    expect(document.body.textContent).not.toContain('a-new-password-1');
  });
});

describe('aliases', () => {
  const AL: Alias = { id: 1, alias: 'postmaster@example.org', target: 'alice@example.org' };

  it('lists, creates and removes aliases', async () => {
    const s = await open('/domains/example.org/aliases', PLATFORM, {
      'GET /api/v1/domains/example.org/aliases': [ok({ items: [AL], next: null }), ok({ items: [AL, { id: 2, alias: 'info@example.org', target: 'alice@example.org' }], next: null })],
      'POST /api/v1/domains/example.org/aliases': created({ alias: 'info@example.org', target: 'alice@example.org' }),
      'DELETE /api/v1/aliases/postmaster%40example.org': ok(),
    });
    expect(await screen.findByText('postmaster@example.org')).toBeTruthy();
    await type(t('aliases.local'), 'info');
    await type(t('aliases.targetAddress'), 'alice@example.org');
    await fireEvent.click(screen.getByRole('button', { name: t('common.create') }));
    expect(await screen.findByText('info@example.org')).toBeTruthy();
    expect(s.last('POST /api/v1/domains/example.org/aliases')!.body).toEqual({ local: 'info', target: 'alice@example.org' });
    await fireEvent.click(screen.getByRole('button', { name: t('aliases.remove', { alias: 'postmaster@example.org' }) }));
    await waitFor(() => expect(screen.queryByText('postmaster@example.org')).toBeNull());
  });
});

describe('dkim', () => {
  const K: Dkim = { selector: 'rusty-ed25519', algorithm: 'ed25519', record_name: 'rusty-ed25519._domainkey.example.org', record_value: 'v=DKIM1; k=ed25519; p=AAAA', sealed: true };

  it('shows the public record in selectable fields and says the private key is sealed, never showing one', async () => {
    await open('/domains/example.org/dkim', PLATFORM, { 'GET /api/v1/domains/example.org/dkim': ok({ items: [K] }) });
    expect(await screen.findByText(t('dkim.sealed'))).toBeTruthy();
    const value = screen.getByLabelText(t('dkim.recordValue')) as HTMLInputElement;
    expect(value.readOnly).toBe(true);
    expect(value.value).toBe(K.record_value);
    expect((screen.getByLabelText(t('dkim.recordName')) as HTMLInputElement).value).toBe(K.record_name);
    expect(document.body.innerHTML).not.toContain('PRIVATE');
  });

  it('makes a key with an optional selector and shows its record afterwards', async () => {
    const s = await open('/domains/example.org/dkim', PLATFORM, {
      'GET /api/v1/domains/example.org/dkim': [ok({ items: [] }), ok({ items: [K] })],
      'POST /api/v1/domains/example.org/dkim': created(K),
    });
    expect(await screen.findByText(t('dkim.empty'))).toBeTruthy();
    await fireEvent.click(screen.getByRole('button', { name: t('dkim.new') }));
    expect(await screen.findByText(t('dkim.created', { selector: K.selector }))).toBeTruthy();
    expect(s.last('POST /api/v1/domains/example.org/dkim')!.body).toEqual({});
    expect(await screen.findByText(t('dkim.sealed'))).toBeTruthy();
  });

  it('sends the selector when one is typed and shows a refusal for a published one', async () => {
    const s = await open('/domains/example.org/dkim', PLATFORM, { 'GET /api/v1/domains/example.org/dkim': ok({ items: [] }), 'POST /api/v1/domains/example.org/dkim': fail(409, 'exists') });
    await type(t('dkim.newSelector'), 's2');
    await fireEvent.click(await screen.findByRole('button', { name: t('dkim.new') }));
    expect((await screen.findByRole('alert')).textContent).toBe(t('error.exists'));
    expect(s.last('POST /api/v1/domains/example.org/dkim')!.body).toEqual({ selector: 's2' });
  });

  it('says plainly when sealing is not set up on the server', async () => {
    await open('/domains/example.org/dkim', PLATFORM, { 'GET /api/v1/domains/example.org/dkim': ok({ items: [] }), 'POST /api/v1/domains/example.org/dkim': fail(503, 'unavailable') });
    await fireEvent.click(await screen.findByRole('button', { name: t('dkim.new') }));
    expect((await screen.findByRole('alert')).textContent).toBe(t('error.unavailable'));
  });
});

describe('dns', () => {
  const R = (over: Partial<DnsRecord>): DnsRecord => ({ name: 'example.org.', type: 'MX', value: '10 mail.example.org.', ttl: 3600, purpose: 'Where mail is delivered', required: 'required', status: 'unchecked', seen: null, last_checked_at: null, ...over });

  it('shows each record with its needed-ness and the status of its last check, in words from the catalogue', async () => {
    await open('/domains/example.org/dns', PLATFORM, {
      'GET /api/v1/domains/example.org/dns': ok({
        records: [R({}), R({ type: 'TXT', name: 'example.org.', value: 'v=spf1 -all', purpose: 'SPF', status: 'mismatch', seen: 'v=spf1 +all', last_checked_at: 1_700_000_000, required: 'recommended' }), R({ type: 'A', status: 'ok', last_checked_at: 1_700_000_000, required: 'optional', value: '203.0.113.5' })],
        warnings: ['no_public_ips'],
      }),
    });
    const table = await screen.findByRole('table', { name: t('dns.list') });
    expect(within(table).getByText(t('dns.status.unchecked'))).toBeTruthy();
    expect(within(table).getByText(t('dns.status.mismatch'))).toBeTruthy();
    expect(within(table).getByText(t('dns.status.ok'))).toBeTruthy();
    expect(within(table).getByText(t('dns.seen', { seen: 'v=spf1 +all' }))).toBeTruthy();
    expect(within(table).getByText(t('dns.required.recommended'))).toBeTruthy();
    expect(screen.getByText(t('dns.warning.no_public_ips'))).toBeTruthy();
    expect((within(table).getAllByLabelText(t('dns.value'))[0] as HTMLInputElement).value).toBe('10 mail.example.org.');
  });

  it('queues a check and offers a reload', async () => {
    const s = await open('/domains/example.org/dns', PLATFORM, {
      'GET /api/v1/domains/example.org/dns': ok({ records: [R({})], warnings: [] }),
      'POST /api/v1/domains/example.org/dns/check': ok({ queued: true }),
    });
    await fireEvent.click(await screen.findByRole('button', { name: t('dns.check') }));
    expect(await screen.findByText(t('dns.checkQueued'))).toBeTruthy();
    expect(s.last('POST /api/v1/domains/example.org/dns/check')!.headers['X-CSRF-Token']).toBe(PLATFORM.csrf);
  });

  it('shows an unknown status as undecided, never as the raw value from the server', async () => {
    await open('/domains/example.org/dns', PLATFORM, { 'GET /api/v1/domains/example.org/dns': ok({ records: [R({ status: '<script>x</script>' })], warnings: [] }) });
    expect(await screen.findByText(t('dns.status.unknown'))).toBeTruthy();
    expect(document.body.innerHTML).not.toContain('<script>x');
  });
});

describe('relay', () => {
  const REL: Relay = { host: 'smtp.mail.net', port: 587, tls: 'starttls', username: 'mailer', spf_include: 'spf.mail.net', password_set: true, check_status: 'auth_failed', checked_at: 1_700_000_000 };

  it('shows the relay without its password, with the result of the last test', async () => {
    await open('/domains/example.org/relay', PLATFORM, { 'GET /api/v1/domains/example.org/relay': ok({ relay: REL }) });
    expect(await screen.findByText(t('relay.passwordSet'))).toBeTruthy();
    expect(screen.getByText(t('relay.check.auth_failed'))).toBeTruthy();
    expect(field(t('relay.host')).value).toBe('smtp.mail.net');
    expect(field(t('relay.username')).value).toBe('mailer');
    expect(field(t('relay.password')).value).toBe('');
  });

  it('says there is no relay of its own and sends a plain relay without any password field', async () => {
    const s = await open('/domains/example.org/relay', PLATFORM, {
      'GET /api/v1/domains/example.org/relay': ok({ relay: null }),
      'PUT /api/v1/domains/example.org/relay': ok({ relay: { ...REL, username: null, password_set: false, check_status: null, checked_at: null } }),
    });
    expect(await screen.findByText(t('relay.none'))).toBeTruthy();
    await type(t('relay.host'), 'relay.mail.net');
    await fireEvent.click(screen.getByRole('button', { name: t('relay.save') }));
    expect(await screen.findByText(t('relay.saved'))).toBeTruthy();
    expect(s.last('PUT /api/v1/domains/example.org/relay')!.body).toEqual({ host: 'relay.mail.net', port: 587, tls: 'starttls' });
  });

  it('sends the password with a user name once and keeps it neither in the field nor on the page', async () => {
    const s = await open('/domains/example.org/relay', PLATFORM, {
      'GET /api/v1/domains/example.org/relay': ok({ relay: null }),
      'PUT /api/v1/domains/example.org/relay': ok({ relay: REL }),
    });
    await screen.findByText(t('relay.none'));
    await type(t('relay.host'), 'smtp.mail.net');
    await type(t('relay.username'), 'mailer');
    await type(t('relay.password'), 'relay-secret-1');
    await fireEvent.click(screen.getByRole('button', { name: t('relay.save') }));
    await screen.findByText(t('relay.saved'));
    expect(s.last('PUT /api/v1/domains/example.org/relay')!.body).toEqual({ host: 'smtp.mail.net', port: 587, tls: 'starttls', username: 'mailer', password: 'relay-secret-1' });
    expect(field(t('relay.password')).value).toBe('');
    expect(document.body.textContent).not.toContain('relay-secret-1');
  });

  it('shows a policy refusal with the fixed message', async () => {
    await open('/domains/example.org/relay', PLATFORM, { 'GET /api/v1/domains/example.org/relay': ok({ relay: null }), 'PUT /api/v1/domains/example.org/relay': fail(400, 'invalid') });
    await type(t('relay.host'), '10.0.0.5');
    await fireEvent.click(await screen.findByRole('button', { name: t('relay.save') }));
    expect((await screen.findByRole('alert')).textContent).toBe(t('error.invalid'));
  });

  it('queues a test, and removes the relay', async () => {
    const s = await open('/domains/example.org/relay', PLATFORM, {
      'GET /api/v1/domains/example.org/relay': ok({ relay: REL }),
      'POST /api/v1/domains/example.org/relay/check': ok({ queued: true }),
      'DELETE /api/v1/domains/example.org/relay': ok(),
    });
    await fireEvent.click(await screen.findByRole('button', { name: t('relay.check') }));
    expect(await screen.findByText(t('relay.checkQueued'))).toBeTruthy();
    await fireEvent.click(screen.getByRole('button', { name: t('relay.remove') }));
    expect(await screen.findByText(t('relay.removed'))).toBeTruthy();
    expect(screen.getByText(t('relay.none'))).toBeTruthy();
    expect(s.last('DELETE /api/v1/domains/example.org/relay')!.headers['X-CSRF-Token']).toBe(PLATFORM.csrf);
  });

  it('shows a result category it does not know as unreachable', async () => {
    await open('/domains/example.org/relay', PLATFORM, { 'GET /api/v1/domains/example.org/relay': ok({ relay: { ...REL, check_status: 'something_new' } }) });
    expect(await screen.findByText(t('relay.check.unreachable'))).toBeTruthy();
  });
});

describe('navigation', () => {
  it('says not found for an address that is no page', async () => {
    await open('/domains/example.org/nothing', PLATFORM, {});
    expect(await screen.findByRole('heading', { name: t('notfound.title') })).toBeTruthy();
  });

  it('follows a link without reloading and the page title changes with the route', async () => {
    await open('/', PLATFORM, { 'GET /api/v1/domains': ok({ items: [D1], next: null }), 'GET /api/v1/domains/example.org/outbound': ok({ queued: 0, running: 0, failed: 0, dead: 0 }) });
    await fireEvent.click(await screen.findByRole('link', { name: 'example.org' }));
    expect(nav.path).toBe('/domains/example.org/overview');
    expect(await screen.findByRole('heading', { name: 'example.org' })).toBeTruthy();
  });
});

describe('storage', () => {
  const ST: Storage = {
    kind: 's3',
    ready: false,
    check_status: 'auth',
    checked_at: 1_700_000_000,
    settings: { endpoint: 'https://s3.example-cloud.com', region: 'eu-west-3', bucket: 'tenant-mail', prefix: 'mail/a', path_style: true },
    access_key_set: true,
    secret_key_set: true,
    policy: null,
    backends: null,
  };
  const POOL: Storage = {
    kind: 'pool',
    ready: false,
    check_status: 'unchecked',
    checked_at: null,
    settings: null,
    access_key_set: null,
    secret_key_set: null,
    policy: { copies: 2, sync_copies: 1, placement: 'ordered', read: 'ordered' },
    backends: [
      { name: 'a', state: 'active', weight: 100, check_status: 'ok', checked_at: 1_700_000_000, settings: { endpoint: 'https://s3.example-cloud.com', region: 'r', bucket: 'bucket-a', prefix: '', path_style: true }, access_key_set: true, secret_key_set: true, objects: 7 },
      { name: 'b', state: 'draining', weight: 300, check_status: 'auth', checked_at: 1_700_000_000, settings: { endpoint: 'https://s3.example-cloud.com', region: 'r', bucket: 'bucket-b', prefix: '', path_style: true }, access_key_set: true, secret_key_set: true, objects: 0 },
    ],
  };
  const GET = 'GET /api/v1/domains/example.org/storage';
  const PUT = 'PUT /api/v1/domains/example.org/storage';

  it('shows a platform administrator the settings, the result of the last test and no keys', async () => {
    await open('/domains/example.org/storage', PLATFORM, { [GET]: ok({ storage: ST }) });
    expect(await screen.findByText(t('storage.notReady'))).toBeTruthy();
    expect(screen.getByText(t('storage.check.auth'))).toBeTruthy();
    expect(screen.getByText(t('storage.keysSet'))).toBeTruthy();
    expect(field(t('storage.bucket')).value).toBe('tenant-mail');
    expect(field(t('storage.accessKey')).value).toBe('');
    expect(field(t('storage.secretKey')).value).toBe('');
  });

  it('sends the settings and the keys once and keeps the keys neither in the fields nor on the page', async () => {
    const s = await open('/domains/example.org/storage', PLATFORM, { [GET]: ok({ storage: null }), [PUT]: ok({ storage: { ...ST, check_status: 'unchecked', checked_at: null } }) });
    await screen.findByText(t('storage.none'));
    await type(t('storage.endpoint'), 'https://s3.example-cloud.com');
    await type(t('storage.region'), 'eu-west-3');
    await type(t('storage.bucket'), 'tenant-mail');
    await type(t('storage.accessKey'), 'AKIAEXAMPLEKEY');
    await type(t('storage.secretKey'), 'super-secret-key-material');
    await fireEvent.click(screen.getByRole('button', { name: t('storage.save') }));
    await screen.findByText(t('storage.saved'));
    expect(s.last(PUT)!.body).toEqual({ endpoint: 'https://s3.example-cloud.com', region: 'eu-west-3', bucket: 'tenant-mail', prefix: '', path_style: false, access_key: 'AKIAEXAMPLEKEY', secret_key: 'super-secret-key-material' });
    expect(field(t('storage.accessKey')).value).toBe('');
    expect(field(t('storage.secretKey')).value).toBe('');
    expect(document.body.textContent).not.toContain('super-secret-key-material');
  });

  it('shows a refusal with the fixed message', async () => {
    await open('/domains/example.org/storage', PLATFORM, { [GET]: ok({ storage: null }), [PUT]: fail(400, 'invalid') });
    await type(t('storage.endpoint'), 'http://127.0.0.1');
    await type(t('storage.region'), 'r');
    await type(t('storage.bucket'), 'bucket');
    await type(t('storage.accessKey'), 'a');
    await type(t('storage.secretKey'), 'b');
    await fireEvent.click(screen.getByRole('button', { name: t('storage.save') }));
    expect((await screen.findByRole('alert')).textContent).toBe(t('error.invalid'));
  });

  it('queues a test of the store', async () => {
    const s = await open('/domains/example.org/storage', PLATFORM, { [GET]: ok({ storage: ST }), 'POST /api/v1/domains/example.org/storage/check': ok({ queued: true }) });
    await fireEvent.click(await screen.findByRole('button', { name: t('storage.check') }));
    expect(await screen.findByText(t('storage.checkQueued'))).toBeTruthy();
    expect(s.last('POST /api/v1/domains/example.org/storage/check')!.headers['X-CSRF-Token']).toBe(PLATFORM.csrf);
  });

  it('shows a domain administrator only the status, with no form', async () => {
    await open('/domains/example.org/storage', TENANT, { 'GET /api/v1/domains': ok({ items: [D1], next: null }), [GET]: ok({ storage: { ...ST, settings: null, access_key_set: null, secret_key_set: null } }) });
    expect(await screen.findByText(t('storage.platformOnly'))).toBeTruthy();
    expect(screen.getByText(t('storage.notReady'))).toBeTruthy();
    expect(screen.queryByLabelText(t('storage.bucket'))).toBeNull();
    expect(screen.queryByRole('button', { name: t('storage.save') })).toBeNull();
    expect(screen.queryByRole('button', { name: t('storage.check') })).toBeNull();
  });

  const POOL_PUT = 'PUT /api/v1/domains/example.org/storage/pool';

  it('lets a platform administrator choose a pool for a domain without a store and shows its backends', async () => {
    const s = await open('/domains/example.org/storage', PLATFORM, { [GET]: ok({ storage: null }), [POOL_PUT]: ok({ storage: { ...POOL, backends: [] } }) });
    await fireEvent.click(await screen.findByRole('button', { name: t('pool.use') }));
    await fireEvent.input(await screen.findByLabelText(t('pool.copies')), { target: { value: '2' } });
    await fireEvent.input(screen.getByLabelText(t('pool.syncCopies')), { target: { value: '1' } });
    await fireEvent.click(screen.getByRole('button', { name: t('pool.save') }));
    expect(await screen.findByText(t('pool.saved'))).toBeTruthy();
    expect(s.last(POOL_PUT)!.body).toEqual({ copies: 2, sync_copies: 1, placement: 'weighted', read: 'located' });
    expect(screen.getByText(t('pool.noBackends'))).toBeTruthy();
  });

  it('shows the state of every backend of a pool, with the result of the last test and the objects held', async () => {
    await open('/domains/example.org/storage', PLATFORM, { [GET]: ok({ storage: POOL }) });
    const table = await screen.findByRole('table');
    expect(within(table).getByText(t('storage.check.auth'))).toBeTruthy();
    expect(within(table).getByText(t('pool.state.draining'))).toBeTruthy();
    expect(within(table).getByText('7')).toBeTruthy();
    expect(screen.getByRole('button', { name: t('storage.check') })).toBeTruthy();
  });

  it('adds a backend with its keys sent once and kept neither in the fields nor on the page', async () => {
    const add = 'POST /api/v1/domains/example.org/storage/backends';
    const s = await open('/domains/example.org/storage', PLATFORM, { [GET]: ok({ storage: POOL }), [add]: ok({ storage: POOL }) });
    await type(t('pool.name'), 'c');
    await type(t('storage.endpoint'), 'https://s3.example-cloud.com');
    await type(t('storage.region'), 'eu-west-3');
    await type(t('storage.bucket'), 'bucket-c');
    const keys = screen.getAllByLabelText(t('storage.accessKey')) as HTMLInputElement[];
    const secrets = screen.getAllByLabelText(t('storage.secretKey')) as HTMLInputElement[];
    await fireEvent.input(keys[keys.length - 1]!, { target: { value: 'AKIAEXAMPLEKEY' } });
    await fireEvent.input(secrets[secrets.length - 1]!, { target: { value: 'super-secret-key-material' } });
    await fireEvent.click(screen.getByRole('button', { name: t('pool.addBackendSave') }));
    expect(await screen.findByText(t('pool.backendAdded'))).toBeTruthy();
    expect(s.last(add)!.body).toEqual({ name: 'c', endpoint: 'https://s3.example-cloud.com', region: 'eu-west-3', bucket: 'bucket-c', prefix: '', path_style: false, weight: 100, access_key: 'AKIAEXAMPLEKEY', secret_key: 'super-secret-key-material' });
    expect((screen.getAllByLabelText(t('storage.secretKey')) as HTMLInputElement[]).every((i) => i.value === '')).toBe(true);
    expect(document.body.textContent).not.toContain('super-secret-key-material');
  });

  it('changes the state of a backend and removes one', async () => {
    const put = 'PUT /api/v1/domains/example.org/storage/backends/a';
    const del = 'DELETE /api/v1/domains/example.org/storage/backends/b';
    const s = await open('/domains/example.org/storage', PLATFORM, { [GET]: ok({ storage: POOL }), [put]: ok({ storage: POOL }), [del]: ok() });
    const row = (await screen.findByText('a', { selector: 'legend' })).closest('fieldset')!;
    await fireEvent.change(within(row).getByLabelText(t('pool.state')), { target: { value: 'readonly' } });
    await fireEvent.click(within(row).getByRole('button', { name: t('pool.update') }));
    expect(await screen.findByText(t('pool.backendUpdated'))).toBeTruthy();
    expect(s.last(put)!.body).toEqual({ state: 'readonly', weight: 100 });
    const other = screen.getByText('b', { selector: 'legend' }).closest('fieldset')!;
    await fireEvent.click(within(other).getByRole('button', { name: t('pool.remove') }));
    expect(await screen.findByText(t('pool.backendRemoved'))).toBeTruthy();
    expect(s.last(del)).toBeTruthy();
  });

  it('shows a domain administrator the backends and none of the settings or controls', async () => {
    const seen: Storage = { ...POOL, backends: (POOL.backends ?? []).map((b) => ({ ...b, settings: null, access_key_set: null, secret_key_set: null, objects: null })) };
    await open('/domains/example.org/storage', TENANT, { 'GET /api/v1/domains': ok({ items: [D1], next: null }), [GET]: ok({ storage: seen }) });
    const table = await screen.findByRole('table');
    expect(within(table).getByText('a')).toBeTruthy();
    expect(document.body.textContent).not.toContain('bucket-a');
    expect(screen.queryByRole('button', { name: t('pool.save') })).toBeNull();
    expect(screen.queryByRole('button', { name: t('pool.remove') })).toBeNull();
    expect(screen.getByText(t('storage.platformOnly'))).toBeTruthy();
  });
});

describe('administrators', () => {
  const ROOT = { id: 1, address: 'root@example.org', scope: 'platform', domain: null, created_at: 1700000000 };
  const CAROL = { id: 2, address: 'carol@example.org', scope: 'domain', domain: 'example.org', created_at: 1700000100 };

  it('lists the administrators with what each may manage and removes one, but never offers to remove oneself', async () => {
    const s = await open('/administrators', PLATFORM, {
      'GET /api/v1/admins': ok({ items: [ROOT, CAROL], next: null }),
      'GET /api/v1/domains': ok({ items: [D1], next: null }),
      'DELETE /api/v1/admins/carol%40example.org': ok(),
    });
    const table = await screen.findByRole('table', { name: t('admins.list') });
    expect(within(table).getByText(t('admins.scopePlatform'))).toBeTruthy();
    expect(within(table).getByText(t('admins.scopeDomain', { domain: 'example.org' }))).toBeTruthy();
    expect(screen.queryByRole('button', { name: t('admins.remove', { address: 'root@example.org' }) })).toBeNull();
    await fireEvent.click(screen.getByRole('button', { name: t('admins.remove', { address: 'carol@example.org' }) }));
    expect(await screen.findByText(t('admins.removed', { address: 'carol@example.org' }))).toBeTruthy();
    expect(s.last('DELETE /api/v1/admins/carol%40example.org')?.headers['X-CSRF-Token']).toBe(PLATFORM.csrf);
  });

  it('adds an administrator limited to a domain, or to every domain', async () => {
    const s = await open('/administrators', PLATFORM, {
      'GET /api/v1/admins': [ok({ items: [ROOT], next: null }), ok({ items: [ROOT, CAROL], next: null })],
      'GET /api/v1/domains': ok({ items: [D1], next: null }),
      'POST /api/v1/admins': created(CAROL),
    });
    await type(t('admins.address'), 'carol@example.org');
    await screen.findByRole('option', { name: 'example.org' });
    await fireEvent.change(screen.getByLabelText(t('admins.domain')), { target: { value: 'example.org' } });
    await fireEvent.click(screen.getByRole('button', { name: t('admins.add') }));
    expect(await screen.findByText(t('admins.added', { address: 'carol@example.org' }))).toBeTruthy();
    expect(s.last('POST /api/v1/admins')?.body).toEqual({ address: 'carol@example.org', domain: 'example.org' });
  });

  it('says why the last platform administrator cannot be removed and why an account without a password cannot be added', async () => {
    await open('/administrators', PLATFORM, {
      'GET /api/v1/admins': ok({ items: [ROOT, { ...CAROL, id: 3, address: 'dave@example.org', scope: 'platform', domain: null }], next: null }),
      'GET /api/v1/domains': ok({ items: [D1], next: null }),
      'DELETE /api/v1/admins/dave%40example.org': fail(409, 'exists'),
      'POST /api/v1/admins': fail(400, 'invalid'),
    });
    await fireEvent.click(await screen.findByRole('button', { name: t('admins.remove', { address: 'dave@example.org' }) }));
    expect(await screen.findByText(t('admins.last'))).toBeTruthy();
    await type(t('admins.address'), 'nopw@example.org');
    await fireEvent.click(screen.getByRole('button', { name: t('admins.add') }));
    expect(await screen.findByText(t('admins.noPassword'))).toBeTruthy();
  });

  it('shows a domain administrator the list and nothing to change', async () => {
    await open('/administrators', TENANT, { 'GET /api/v1/admins': ok({ items: [CAROL], next: null }) });
    await screen.findByRole('table', { name: t('admins.list') });
    expect(screen.queryByRole('button', { name: t('common.remove') })).toBeNull();
    expect(screen.queryByText(t('admins.new'))).toBeNull();
  });
});
