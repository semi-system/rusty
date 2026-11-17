<script lang="ts">
  import PageHead from '../lib/PageHead.svelte';
  import Badge from '../lib/Badge.svelte';
  import { onMount } from 'svelte';
  import type { App } from '../lib/app.svelte';
  import type { Monitor, MonitorInstance } from '../api/types';
  import { Load } from '../lib/load.svelte';
  import ErrorNote from '../lib/ErrorNote.svelte';
  import { labelled } from '../lib/stack';
  import { t, tFor } from '../i18n';

  let { app }: { app: App } = $props();

  // The services beat every 15 seconds; the page asks again at the same pace.
  const EVERY_MS = 15_000;
  const load = new Load<Monitor>(() => app.client.monitor());
  let refreshing = $state(false);

  async function refresh(): Promise<void> {
    refreshing = true;
    try {
      await load.run();
    } finally {
      refreshing = false;
    }
  }

  onMount(() => {
    void refresh();
    const timer = setInterval(() => void refresh(), EVERY_MS);
    // The stream says `monitor` changed every few seconds; the timer stays as the way to refresh without a stream.
    const stop = app.live.watch(['monitor'], () => void load.run());
    return () => {
      clearInterval(timer);
      stop();
    };
  });

  /** A length of time in the two largest units that matter: `2 min`, `1 h 2 min`, `1 d 1 h`. */
  function span(secs: number): string {
    const n = Math.max(0, Math.floor(secs));
    if (n < 60) return t('monitor.duration.seconds', { n });
    const minutes = Math.floor(n / 60);
    if (minutes < 60) return t('monitor.duration.minutes', { n: minutes });
    const hours = Math.floor(minutes / 60);
    if (hours < 24) return t('monitor.duration.hoursMinutes', { h: hours, m: minutes % 60 });
    return t('monitor.duration.daysHours', { d: Math.floor(hours / 24), h: hours % 24 });
  }

  function stateText(i: MonitorInstance): string {
    if (i.replaced) return t('monitor.instance.replaced');
    if (i.stale) return t('monitor.instance.stale');
    return i.status === 'ok' ? t('monitor.instance.ok') : t('monitor.instance.degraded');
  }

  const QUEUE = ['queued', 'running', 'failed', 'dead'] as const;
</script>

<main id="content" tabindex="-1">
  <PageHead title={t('monitor.title')} lede={t('monitor.lede')}>
    {#snippet actions()}
      <button type="button" class="secondary" onclick={refresh} disabled={refreshing}>{refreshing ? t('monitor.refreshing') : t('monitor.refresh')}</button>
    {/snippet}
  </PageHead>
  {#if load.state.status === 'error'}<ErrorNote error={load.state.error} />{/if}
  {#if load.state.status === 'loading'}
    <p class="muted" role="status">{t('app.loading')}</p>
  {:else if load.state.status === 'ready'}
    {@const m = load.state.data}
    <section aria-labelledby="mon-services">
      <div class="section-head"><h2 id="mon-services">{t('monitor.services')}</h2></div>
      <div class="table-wrap">
        <table use:labelled aria-label={t('monitor.services')}>
          <thead>
            <tr><th scope="col">{t('monitor.col.service')}</th><th scope="col">{t('monitor.col.status')}</th><th scope="col">{t('monitor.col.instances')}</th></tr>
          </thead>
          <tbody>
            {#each m.services as s (s.service)}
              <tr>
                <th scope="row">{s.service}</th>
                <td><Badge tone={s.status === 'ok' ? 'ok' : s.status === 'degraded' ? 'warn' : 'danger'}>{tFor('monitor.status.', s.status, 'monitor.status.down')}</Badge></td>
                <td>{#if s.instances.length === 0}{t('monitor.noInstances')}{:else}{s.instances.filter((i) => !i.stale).length}{/if}</td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    </section>

    <section aria-labelledby="mon-instances">
      <div class="section-head"><h2 id="mon-instances">{t('monitor.instances')}</h2></div>
      <div class="table-wrap">
        <table use:labelled aria-label={t('monitor.instances')}>
          <thead>
            <tr>
              <th scope="col">{t('monitor.col.instance')}</th><th scope="col">{t('monitor.col.service')}</th><th scope="col">{t('monitor.col.version')}</th>
              <th scope="col">{t('monitor.col.uptime')}</th><th scope="col">{t('monitor.col.lastBeat')}</th><th scope="col">{t('monitor.col.listeners')}</th><th scope="col">{t('monitor.col.state')}</th>
            </tr>
          </thead>
          <tbody>
            {#each m.services as s (s.service)}
              {#each s.instances as i (s.service + '/' + i.instance)}
                <tr>
                  <th scope="row"><span class="mono">{i.instance}</span></th>
                  <td>{s.service}</td>
                  <td><span class="mono">{i.version}</span></td>
                  <td>{span(i.uptime_secs)}</td>
                  <td>{t('monitor.ago', { age: span(i.age_secs) })}</td>
                  <td>{i.listeners ?? ''}</td>
                  <td><Badge tone={i.replaced ? 'neutral' : i.stale ? 'danger' : i.status === 'ok' ? 'ok' : 'warn'}>{stateText(i)}</Badge></td>
                </tr>
              {/each}
            {/each}
          </tbody>
        </table>
      </div>
    </section>

    <section aria-labelledby="mon-queue">
      <div class="section-head"><h2 id="mon-queue">{t('monitor.queue')}</h2></div>
      {#if m.queue.queued + m.queue.running + m.queue.failed + m.queue.dead === 0}
        <div class="empty"><p>{t('monitor.queue.empty')}</p></div>
      {:else}
        <div class="stats">
          {#each QUEUE as k (k)}
            <div class={(k === 'failed' && m.queue[k] > 0) ? 'stat warn' : (k === 'dead' && m.queue[k] > 0) ? 'stat danger' : 'stat'}>
              <div class="stat-value">{m.queue[k]}</div>
              <div class="stat-label">{tFor('monitor.queue.', k, 'monitor.queue.queued')}</div>
            </div>
          {/each}
        </div>
        {#if m.queue.oldest_due_secs > 0}<p class="muted">{t('monitor.queue.oldest', { age: span(m.queue.oldest_due_secs) })}</p>{/if}
      {/if}
    </section>

    <section aria-labelledby="mon-storage">
      <div class="section-head"><h2 id="mon-storage">{t('monitor.storage')}</h2></div>
      {#if m.storage.length === 0}
        <div class="empty"><p>{t('monitor.storage.empty')}</p></div>
      {:else}
        <ul class="plain checklist">
          {#each m.storage as c (c.status)}
            <li><Badge tone={c.status === 'ok' ? 'ok' : c.status === 'unchecked' ? 'neutral' : 'danger'}>{c.status}</Badge> {t('monitor.storage.count', { count: c.count })}</li>
          {/each}
        </ul>
      {/if}
    </section>

    <section aria-labelledby="mon-dns">
      <div class="section-head"><h2 id="mon-dns">{t('monitor.dns')}</h2></div>
      <p class={m.dns_problem_domains === 0 ? 'alert ok' : 'alert warn'}>{m.dns_problem_domains === 0 ? t('monitor.dns.ok') : t('monitor.dns.problems', { count: m.dns_problem_domains })}</p>
    </section>
  {/if}
</main>
