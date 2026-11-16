import { cleanup, fireEvent, render, screen, within } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import App from '../App.svelte';
import type { Me, Monitor } from '../api/types';
import { App as State } from '../lib/app.svelte';
import { nav } from '../lib/nav.svelte';
import { en } from '../i18n/en';
import { setCatalogue, t } from '../i18n';
import { PLATFORM, TENANT, fail, fakeServer, ok, signedIn, type Handler } from '../test-support';

beforeEach(() => {
  setCatalogue(en, 'en');
  nav.go('/', true);
});
afterEach(cleanup);

const instance = (name: string, over: Partial<Monitor['services'][0]['instances'][0]> = {}) => ({
  instance: name,
  version: '1.2.3',
  uptime_secs: 3725,
  age_secs: 4,
  stale: false,
  status: 'ok',
  listeners: 2,
  ...over,
});

const MONITOR: Monitor = {
  services: [
    { service: 'smtp-in', status: 'ok', instances: [instance('smtp-in-1')] },
    { service: 'submission', status: 'ok', instances: [instance('submission-1')] },
    { service: 'imap', status: 'degraded', instances: [instance('imap-1'), instance('imap-2', { stale: true, age_secs: 100 })] },
    { service: 'outbound', status: 'down', instances: [] },
    { service: 'gc', status: 'degraded', instances: [instance('gc-1', { status: 'degraded', listeners: null })] },
    { service: 'admin', status: 'ok', instances: [instance('admin-1', { uptime_secs: 90_000 })] },
  ],
  queue: { queued: 3, running: 1, failed: 0, dead: 2, oldest_due_secs: 125 },
  storage: [
    { status: 'ok', count: 2 },
    { status: 'failed', count: 1 },
  ],
  dns_problem_domains: 4,
};

async function open(me: Me, routes: Record<string, Handler>) {
  nav.go('/monitor', true);
  const s = fakeServer(signedIn(me, routes));
  render(App, { app: new State(s.client) });
  return s;
}

describe('monitor', () => {
  it('shows every service with its status in words and its instances', async () => {
    await open(PLATFORM, { 'GET /api/v1/monitor': ok(MONITOR) });
    const table = await screen.findByRole('table', { name: t('monitor.services') });
    const rows = within(table).getAllByRole('row');
    // the header and the six services
    expect(rows).toHaveLength(7);
    const row = (service: string) => rows.find((r) => within(r).queryByText(service) !== null) as HTMLElement;
    expect(within(row('smtp-in')).getByText(t('monitor.status.ok'))).toBeTruthy();
    expect(within(row('imap')).getByText(t('monitor.status.degraded'))).toBeTruthy();
    expect(within(row('outbound')).getByText(t('monitor.status.down'))).toBeTruthy();
    expect(within(row('outbound')).getByText(t('monitor.noInstances'))).toBeTruthy();
  });

  it('lists the instances with version, uptime, last beat and what is wrong with a stale or degraded one', async () => {
    await open(PLATFORM, { 'GET /api/v1/monitor': ok(MONITOR) });
    const inst = await screen.findByRole('table', { name: t('monitor.instances') });
    const rows = within(inst).getAllByRole('row');
    const row = (name: string) => rows.find((r) => within(r).queryByText(name) !== null) as HTMLElement;
    expect(within(row('imap-1')).getByText('1 h 2 min')).toBeTruthy();
    expect(within(row('admin-1')).getByText('1 d 1 h')).toBeTruthy();
    expect(within(row('imap-2')).getByText(t('monitor.instance.stale'))).toBeTruthy();
    expect(within(row('gc-1')).getByText(t('monitor.instance.degraded'))).toBeTruthy();
    expect(within(row('smtp-in-1')).getByText(t('monitor.instance.ok'))).toBeTruthy();
  });

  it('shows the queue, the storage checks and the DNS problems', async () => {
    await open(PLATFORM, { 'GET /api/v1/monitor': ok(MONITOR) });
    const queue = await screen.findByRole('region', { name: t('monitor.queue') });
    expect(within(queue).getByText(t('monitor.queue.oldest', { age: '2 min' }))).toBeTruthy();
    expect(within(queue).getByText(t('monitor.queue.dead'))).toBeTruthy();
    const storage = screen.getByRole('region', { name: t('monitor.storage') });
    expect(within(storage).getByText('failed')).toBeTruthy();
    expect(screen.getByText(t('monitor.dns.problems', { count: 4 }))).toBeTruthy();
  });

  it('says so when nothing is wrong', async () => {
    const calm: Monitor = {
      services: MONITOR.services.map((s) => ({ ...s, status: 'ok', instances: [instance(`${s.service}-1`)] })),
      queue: { queued: 0, running: 0, failed: 0, dead: 0, oldest_due_secs: 0 },
      storage: [],
      dns_problem_domains: 0,
    };
    await open(PLATFORM, { 'GET /api/v1/monitor': ok(calm) });
    expect(await screen.findByText(t('monitor.queue.empty'))).toBeTruthy();
    expect(screen.getByText(t('monitor.storage.empty'))).toBeTruthy();
    expect(screen.getByText(t('monitor.dns.ok'))).toBeTruthy();
  });

  it('loads again on request and keeps showing the data while it does', async () => {
    const s = await open(PLATFORM, { 'GET /api/v1/monitor': [ok(MONITOR), ok({ ...MONITOR, queue: { ...MONITOR.queue, queued: 9 } })] });
    await screen.findByRole('table', { name: t('monitor.services') });
    await fireEvent.click(screen.getByRole('button', { name: t('monitor.refresh') }));
    expect(await screen.findByText('9')).toBeTruthy();
    expect(s.calls.filter((c) => c.key === 'GET /api/v1/monitor')).toHaveLength(2);
  });

  it('shows the error note when the server cannot answer', async () => {
    await open(PLATFORM, { 'GET /api/v1/monitor': fail(503, 'busy') });
    expect(await screen.findByRole('alert')).toBeTruthy();
  });

  it('is for platform administrators: the link is not offered to a domain administrator and the page is not found', async () => {
    await open(TENANT, { 'GET /api/v1/domains': ok({ items: [], next: null }) });
    await screen.findByRole('heading', { level: 1 });
    expect(screen.queryByRole('link', { name: t('nav.monitor') })).toBeNull();
    expect(screen.queryByRole('table', { name: t('monitor.services') })).toBeNull();
  });

  it('offers the link to a platform administrator', async () => {
    await open(PLATFORM, { 'GET /api/v1/monitor': ok(MONITOR) });
    const link = await screen.findByRole('link', { name: t('nav.monitor') });
    expect(link.getAttribute('href')).toBe('/monitor');
  });
});
