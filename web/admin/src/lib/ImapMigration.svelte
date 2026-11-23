<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import type { App } from '../lib/app.svelte';
  import type { Migration } from '../api/types';
  import { bytes } from './bytes';
  import ErrorNote from './ErrorNote.svelte';
  import { t, tFor } from '../i18n';

  let { app, address, onclose }: { app: App; address: string; onclose: () => void } = $props();

  let items = $state<Migration[]>([]);
  let loaded = $state(false);
  let error = $state<unknown>(null);
  let notice = $state<string | null>(null);
  let busy = $state(false);
  let invalid = $state(false);
  let host = $state('');
  let port = $state('993');
  let security = $state('tls');
  let user = $state('');
  let password = $state('');
  let timer: ReturnType<typeof setInterval> | null = null;

  const when = (s: number): string => new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'short' }).format(new Date(s * 1000));
  const active = (x: Migration[]): boolean => x.some((i) => i.state === 'preparing' || i.state === 'queued' || i.state === 'running');

  async function load(quiet = false): Promise<void> {
    try {
      items = (await app.client.migrations(address)).items;
    } catch (e) {
      if (!quiet) error = e;
    }
    loaded = true;
    // While one is going on the list refreshes by itself.
    if (active(items) && timer === null) timer = setInterval(() => void load(true), 2000);
    if (!active(items) && timer !== null) {
      clearInterval(timer);
      timer = null;
    }
  }

  onMount(() => void load());
  onDestroy(() => {
    if (timer !== null) clearInterval(timer);
  });

  // The usual port follows the way the connection is protected, until the person types another.
  function secure(e: Event): void {
    const was = security;
    security = (e.currentTarget as HTMLSelectElement).value;
    if ((port === '993' && was === 'tls') || (port === '143' && was !== 'tls')) port = security === 'tls' ? '993' : '143';
  }

  async function start(e: SubmitEvent): Promise<void> {
    e.preventDefault();
    const n = Number(port);
    invalid = host.trim() === '' || !Number.isInteger(n) || n < 1 || n > 65535 || user.trim() === '' || password === '';
    if (invalid) return;
    busy = true;
    error = null;
    notice = null;
    try {
      await app.client.startMigration(address, { host: host.trim(), port: n, security, user: user.trim(), password });
      password = '';
      notice = t('migrate.asked');
      await load(true);
    } catch (err) {
      error = err;
    } finally {
      busy = false;
    }
  }

  async function stop(m: Migration): Promise<void> {
    busy = true;
    error = null;
    try {
      await app.client.stopMigration(address, m.id);
      await load(true);
    } catch (err) {
      error = err;
    } finally {
      busy = false;
    }
  }
</script>

<section class="row-form" aria-label={t('migrate.title', { address })}>
  <h3>{t('migrate.title', { address })}</h3>
  <p class="hint">{t('migrate.lede')}</p>
  {#if error !== null}<ErrorNote {error} />{/if}
  {#if invalid}<p class="alert danger" role="alert">{t('migrate.invalid')}</p>{/if}
  {#if notice !== null}<p class="alert ok" role="status">{notice}</p>{/if}
  <form onsubmit={start}>
    <div class="field-row">
      <label>{t('migrate.host')}<input bind:value={host} name="host" type="text" autocapitalize="none" spellcheck="false" /></label>
      <label>{t('migrate.security')}
        <select name="security" value={security} onchange={secure}>
          <option value="tls">{t('migrate.tls')}</option>
          <option value="starttls">{t('migrate.starttls')}</option>
          <option value="plain">{t('migrate.plain')}</option>
        </select>
      </label>
      <label>{t('migrate.port')}<input bind:value={port} name="port" type="text" inputmode="numeric" /></label>
      <label>{t('migrate.user')}<input bind:value={user} name="user" type="text" autocapitalize="none" spellcheck="false" autocomplete="off" /></label>
      <label>{t('migrate.password')}<input bind:value={password} name="password" type="password" autocomplete="off" /></label>
      <button type="submit" class="primary" disabled={busy}>{t('migrate.start')}</button>
      <button type="button" class="secondary" onclick={onclose}>{t('rules.close')}</button>
    </div>
  </form>
  {#if security === 'plain'}<p class="alert danger" role="alert">{t('migrate.plainWarning')}</p>{/if}
  {#if loaded && items.length === 0}
    <p class="muted">{t('migrate.none')}</p>
  {:else if items.length > 0}
    <ul class="plain" aria-label={t('migrate.list')}>
      {#each items as m (m.id)}
        <li>
          <strong>{m.user}@{m.host}</strong>
          {when(m.created_at)} · {tFor('migrate.state.', m.state, 'migrate.state.queued')}{#if m.stopping} ({t('migrate.stopping')}){/if}
          {#if m.state === 'running' && m.folder !== null} · {t('migrate.folder', { folder: m.folder, done: m.folders_done, total: m.folders_total })}{/if}
          · {t('migrate.counts', { messages: m.messages, skipped: m.skipped, size: bytes(m.bytes) })}
          {#if m.error !== null} · {tFor('migrate.error.', m.error, 'migrate.error.failed')}{/if}
          {#if m.state === 'preparing' || m.state === 'queued' || (m.state === 'running' && !m.stopping)}
            · <button type="button" class="link" disabled={busy} onclick={() => stop(m)}>{t('migrate.stop')}</button>
          {/if}
        </li>
      {/each}
    </ul>
  {/if}
</section>
