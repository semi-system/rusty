<script lang="ts">
  import Badge from '../lib/Badge.svelte';
  import { onMount } from 'svelte';
  import type { App } from '../lib/app.svelte';
  import type { DnsRecords } from '../api/types';
  import Copy from '../lib/Copy.svelte';
  import DomainShell from '../lib/DomainShell.svelte';
  import ErrorNote from '../lib/ErrorNote.svelte';
  import { labelled } from '../lib/stack';
  import { t, tFor } from '../i18n';

  let { app, name }: { app: App; name: string } = $props();

  let data = $state<DnsRecords | null>(null);
  let error = $state<unknown>(null);
  let notice = $state<string | null>(null);
  let busy = $state(false);

  async function load(quiet = false): Promise<void> {
    try {
      data = await app.client.dns(name);
      error = null;
    } catch (e) {
      if (!quiet) error = e;
    }
  }

  onMount(() => {
    void load();
    // A hint refreshes what is shown in the background, without touching what the person is typing in a form.
    return app.live.watch(['dns:' + name.toLowerCase()], () => void load(true));
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
    <div class="section-head">
      <h2>{t('dns.title', { domain: name })}</h2>
      <div class="page-actions">
        <button type="button" onclick={check} disabled={busy}>{t('dns.check')}</button>
        <button type="button" class="secondary" onclick={() => load()}>{t('dns.reload')}</button>
      </div>
    </div>
    <p class="muted">{t('dns.intro')}</p>
    {#if notice !== null}<p class="alert ok" role="status">{notice}</p>{/if}
    {#if error !== null}<ErrorNote {error} />{/if}
    {#if data === null && error === null}
      <p class="muted" role="status">{t('app.loading')}</p>
    {:else if data !== null}
      {#each data.warnings as w (w)}<p class="alert warn">{tFor('dns.warning.', w, 'error.unknown')}</p>{/each}
      <div class="table-wrap">
        <table use:labelled aria-label={t('dns.list')}>
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
                <th scope="row"><span class="mono">{r.name}</span><br /><span class="muted">{r.purpose}</span></th>
                <td><span class="mono">{r.type}</span></td>
                <td><Copy value={r.value} label={t('dns.value')} /></td>
                <td>{tFor('dns.required.', r.required, 'dns.required.optional')}</td>
                <td>
                  <Badge tone={r.status === 'ok' ? 'ok' : r.status === 'warning' || r.status === 'pending' ? 'warn' : r.status === 'missing' || r.status === 'mismatch' ? 'danger' : 'neutral'}>{tFor('dns.status.', r.status, 'dns.status.unknown')}</Badge>
                  {#if r.last_checked_at !== null}<br /><span class="muted">{t('dns.checkedAt')}: {when(r.last_checked_at)}</span>{/if}
                  {#if r.seen !== null}<br /><span class="muted">{t('dns.seen', { seen: r.seen })}</span>{/if}
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    {/if}
  </DomainShell>
</main>
