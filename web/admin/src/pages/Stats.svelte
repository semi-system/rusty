<script lang="ts">
  import { onMount } from 'svelte';
  import type { App } from '../lib/app.svelte';
  import type { DayStats } from '../api/types';
  import DomainShell from '../lib/DomainShell.svelte';
  import ErrorNote from '../lib/ErrorNote.svelte';
  import { labelled } from '../lib/stack';
  import { t } from '../i18n';

  let { app, name }: { app: App; name: string } = $props();

  const day = (s: number): string => new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeZone: 'UTC' }).format(new Date(s * 1000));

  let days = $state<DayStats[]>([]);
  let loaded = $state(false);
  let error = $state<unknown>(null);

  onMount(async () => {
    try {
      days = (await app.client.stats(name)).days;
    } catch (e) {
      error = e;
    }
    loaded = true;
  });
</script>

<main id="content" tabindex="-1">
  <DomainShell {name}>
    <div class="section-head">
      <h2>{t('stats.title', { domain: name })}</h2>
      <p>{t('stats.lede')}</p>
    </div>
    {#if error !== null}<ErrorNote {error} />{/if}
    {#if !loaded}
      <p class="muted" role="status">{t('app.loading')}</p>
    {:else if days.length === 0}
      <div class="empty"><p>{t('stats.empty')}</p></div>
    {:else}
      <div class="table-wrap">
        <table use:labelled aria-label={t('stats.list')}>
          <thead>
            <tr>
              <th scope="col">{t('stats.col.day')}</th>
              <th scope="col">{t('stats.col.received')}</th>
              <th scope="col">{t('stats.col.rejected')}</th>
              <th scope="col">{t('stats.col.sent')}</th>
              <th scope="col">{t('stats.col.deferred')}</th>
              <th scope="col">{t('stats.col.failed')}</th>
            </tr>
          </thead>
          <tbody>
            {#each days as d (d.day)}
              <tr>
                <th scope="row">{day(d.day)}</th>
                <td>{d.received}</td>
                <td>{d.rejected}</td>
                <td>{d.sent}</td>
                <td>{d.deferred}</td>
                <td>{d.failed}</td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    {/if}
  </DomainShell>
</main>
