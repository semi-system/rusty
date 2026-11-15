<script lang="ts">
  import { onMount } from 'svelte';
  import type { App } from '../lib/app.svelte';
  import type { DnsRecords } from '../api/types';
  import Copy from '../lib/Copy.svelte';
  import DomainShell from '../lib/DomainShell.svelte';
  import ErrorNote from '../lib/ErrorNote.svelte';
  import { t, tFor } from '../i18n';

  let { app, name }: { app: App; name: string } = $props();

  let data = $state<DnsRecords | null>(null);
  let error = $state<unknown>(null);
  let notice = $state<string | null>(null);
  let busy = $state(false);

  async function load(): Promise<void> {
    try {
      data = await app.client.dns(name);
      error = null;
    } catch (e) {
      error = e;
    }
  }

  onMount(() => {
    void load();
  });

  async function check(): Promise<void> {
    busy = true;
    error = null;
    notice = null;
    try {
      await app.client.checkDns(name);
      notice = t('dns.checkQueued');
    } catch (e) {
      error = e;
    } finally {
      busy = false;
    }
  }

  const when = (s: number): string => new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'short' }).format(new Date(s * 1000));
</script>

<main id="content" tabindex="-1">
  <DomainShell {name}>
    <h2>{t('dns.title', { domain: name })}</h2>
    <p class="muted">{t('dns.intro')}</p>
    <p>
      <button type="button" onclick={check} disabled={busy}>{t('dns.check')}</button>
      <button type="button" onclick={load}>{t('dns.reload')}</button>
    </p>
    {#if notice !== null}<p role="status">{notice}</p>{/if}
    {#if error !== null}<ErrorNote {error} />{/if}
    {#if data === null && error === null}
      <p class="muted" role="status">{t('app.loading')}</p>
    {:else if data !== null}
      {#each data.warnings as w (w)}<p class="note">{tFor('dns.warning.', w, 'error.unknown')}</p>{/each}
      <table aria-label={t('dns.list')}>
        <thead>
          <tr>
            <th scope="col">{t('dns.name')}</th>
            <th scope="col">{t('dns.type')}</th>
            <th scope="col">{t('dns.value')}</th>
            <th scope="col">{t('dns.requirement')}</th>
            <th scope="col">{t('dns.status')}</th>
          </tr>
        </thead>
        <tbody>
          {#each data.records as r (r.name + '|' + r.type + '|' + r.value)}
            <tr>
              <th scope="row">{r.name}<br /><span class="muted">{r.purpose}</span></th>
              <td>{r.type}</td>
              <td><Copy value={r.value} label={t('dns.value')} /></td>
              <td>{tFor('dns.required.', r.required, 'dns.required.optional')}</td>
              <td>
                {tFor('dns.status.', r.status, 'dns.status.unknown')}
                {#if r.last_checked_at !== null}<br /><span class="muted">{t('dns.checkedAt')}: {when(r.last_checked_at)}</span>{/if}
                {#if r.seen !== null}<br /><span class="muted">{t('dns.seen', { seen: r.seen })}</span>{/if}
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
    {/if}
  </DomainShell>
</main>
