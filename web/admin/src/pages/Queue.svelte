<script lang="ts">
  import { onMount } from 'svelte';
  import type { App } from '../lib/app.svelte';
  import type { LogLine, QueueJob } from '../api/types';
  import Badge from '../lib/Badge.svelte';
  import DomainShell from '../lib/DomainShell.svelte';
  import ErrorNote from '../lib/ErrorNote.svelte';
  import { labelled } from '../lib/stack';
  import { t, tFor } from '../i18n';

  let { app, name }: { app: App; name: string } = $props();

  const when = (s: number): string => new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'short' }).format(new Date(s * 1000));

  let jobs = $state<QueueJob[]>([]);
  let jobsNext = $state<number | null>(null);
  let filter = $state('');
  let search = $state('');
  let loaded = $state(false);
  let error = $state<unknown>(null);
  let busy = $state(false);
  let deleting = $state<number | null>(null);

  let lines = $state<LogLine[]>([]);
  let linesNext = $state<number | null>(null);
  let traceQuery = $state('');
  // Only the mail that was filed in the Spam folder.
  let heldOnly = $state(false);
  let traced = $state(false);

  async function loadJobs(before?: number): Promise<void> {
    try {
      const page = await app.client.queue(name, { state: filter, q: search, before });
      jobs = before === undefined ? page.items : [...jobs, ...page.items];
      jobsNext = page.next;
    } catch (e) {
      error = e;
    }
    loaded = true;
  }

  async function loadTrace(before?: number): Promise<void> {
    try {
      const page = await app.client.trace(name, { q: traceQuery, before, held: heldOnly });
      lines = before === undefined ? page.items : [...lines, ...page.items];
      linesNext = page.next;
    } catch (e) {
      error = e;
    }
    traced = true;
  }

  onMount(() => {
    void loadJobs();
    void loadTrace();
  });

  async function act(job: QueueJob, action: 'hold' | 'release' | 'retry' | 'delete'): Promise<void> {
    busy = true;
    error = null;
    try {
      await app.client.queueAction(name, job.id, action);
      deleting = null;
      await loadJobs();
    } catch (e) {
      error = e;
    } finally {
      busy = false;
    }
  }

  const tone = (s: string): 'ok' | 'warn' | 'danger' | 'neutral' => (s === 'waiting' || s === 'done' ? 'ok' : s === 'dead' || s === 'failed' ? 'danger' : s === 'held' ? 'warn' : 'neutral');
  const eventTone = (e: string): 'ok' | 'warn' | 'danger' => (e === 'delivered' ? 'ok' : e === 'deferred' ? 'warn' : 'danger');
</script>

<main id="content" tabindex="-1">
  <DomainShell {name}>
    <div class="section-head">
      <h2>{t('queue.title', { domain: name })}</h2>
      <p>{t('queue.lede')}</p>
    </div>
    {#if error !== null}<ErrorNote {error} />{/if}

    <form class="field-row" onsubmit={(e) => { e.preventDefault(); void loadJobs(); }}>
      <label>
        {t('queue.state')}
        <select bind:value={filter} onchange={() => void loadJobs()}>
          <option value="">{t('queue.all')}</option>
          {#each ['waiting', 'held', 'running', 'failed', 'dead'] as s (s)}<option value={s}>{tFor('queue.state.', s, 'queue.state.waiting')}</option>{/each}
        </select>
      </label>
      <label>{t('queue.search')}<input bind:value={search} name="q" type="search" autocapitalize="none" spellcheck="false" /></label>
      <button type="submit" class="secondary">{t('queue.find')}</button>
    </form>

    {#if !loaded}
      <p class="muted" role="status">{t('app.loading')}</p>
    {:else if jobs.length === 0}
      <div class="empty"><p>{t('queue.empty')}</p></div>
    {:else}
      <div class="table-wrap">
        <table use:labelled aria-label={t('queue.list')}>
          <thead>
            <tr>
              <th scope="col">{t('queue.recipients')}</th>
              <th scope="col">{t('queue.stateCol')}</th>
              <th scope="col">{t('queue.attempts')}</th>
              <th scope="col">{t('queue.next')}</th>
              <th scope="col">{t('queue.reason')}</th>
              <th scope="col" class="col-actions"><span class="sr">{t('domains.col.action')}</span></th>
            </tr>
          </thead>
          <tbody>
            {#each jobs as j (j.id)}
              <tr>
                <th scope="row">{j.recipients.join(', ')}<br /><span class="muted">{t('queue.from', { sender: j.sender })} · {when(j.created_at)}</span></th>
                <td><Badge tone={tone(j.state)}>{tFor('queue.state.', j.state, 'queue.state.waiting')}</Badge></td>
                <td>{j.attempts}</td>
                <td>{j.next_attempt_at === null ? '' : when(j.next_attempt_at)}</td>
                <td>{j.last_error ?? ''}</td>
                <td class="col-actions">
                  <div class="row-actions">
                    {#if j.state === 'waiting'}
                      <button type="button" disabled={busy} aria-label={t('queue.sendNowFor', { id: j.id })} onclick={() => act(j, 'release')}>{t('queue.sendNow')}</button>
                      <button type="button" disabled={busy} aria-label={t('queue.holdFor', { id: j.id })} onclick={() => act(j, 'hold')}>{t('queue.hold')}</button>
                    {:else if j.state === 'held'}
                      <button type="button" disabled={busy} aria-label={t('queue.releaseFor', { id: j.id })} onclick={() => act(j, 'release')}>{t('queue.release')}</button>
                    {:else if j.state === 'dead' || j.state === 'failed'}
                      <button type="button" disabled={busy} aria-label={t('queue.retryFor', { id: j.id })} onclick={() => act(j, 'retry')}>{t('queue.retry')}</button>
                    {/if}
                    {#if j.state !== 'running' && j.state !== 'done'}
                      <button type="button" disabled={busy} aria-label={t('queue.deleteFor', { id: j.id })} onclick={() => (deleting = deleting === j.id ? null : j.id)}>{t('accounts.deleteShort')}</button>
                    {/if}
                  </div>
                  {#if deleting === j.id}
                    <div class="row-form" role="alert">
                      <p>{t('queue.deleteAsk')}</p>
                      <div class="form-actions">
                        <button type="button" class="danger" disabled={busy} onclick={() => act(j, 'delete')}>{t('queue.deleteYes')}</button>
                        <button type="button" class="secondary" onclick={() => (deleting = null)}>{t('accounts.cancel')}</button>
                      </div>
                    </div>
                  {/if}
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
      {#if jobsNext !== null}<p class="form-actions"><button type="button" class="secondary" onclick={() => loadJobs(jobsNext ?? undefined)}>{t('common.loadMore')}</button></p>{/if}
    {/if}

    <section class="panel" aria-labelledby="trace">
      <h2 id="trace">{t('queue.trace')}</h2>
      <p class="hint">{t('queue.traceLede')}</p>
      <form class="field-row" onsubmit={(e) => { e.preventDefault(); void loadTrace(); }}>
        <label>{t('queue.traceSearch')}<input bind:value={traceQuery} name="trace" type="search" autocapitalize="none" spellcheck="false" /></label>
        <label class="check"><input type="checkbox" bind:checked={heldOnly} name="held" onchange={() => void loadTrace()} /> {t('queue.heldOnly')}</label>
        <button type="submit" class="secondary">{t('queue.find')}</button>
      </form>
      {#if traced && lines.length === 0}
        <div class="empty"><p>{t('queue.traceEmpty')}</p></div>
      {:else if lines.length > 0}
        <div class="table-wrap">
          <table use:labelled aria-label={t('queue.trace')}>
            <thead>
              <tr><th scope="col">{t('queue.when')}</th><th scope="col">{t('queue.direction')}</th><th scope="col">{t('queue.recipient')}</th><th scope="col">{t('queue.event')}</th><th scope="col">{t('queue.detail')}</th></tr>
            </thead>
            <tbody>
              {#each lines as l (l.id)}
                <tr>
                  <td>{when(l.at)}</td>
                  <td>{l.direction === 'in' ? t('queue.received') : t('queue.sent')}</td>
                  <th scope="row">{l.recipient}<br /><span class="muted">{t('queue.from', { sender: l.sender })}</span></th>
                  <td><Badge tone={eventTone(l.event)}>{tFor('queue.event.', l.event, 'queue.event.delivered')}</Badge></td>
                  <td>{[l.remote, l.status, l.detail].filter((x) => x !== null && x !== '').join(' · ')}</td>
                </tr>
              {/each}
            </tbody>
          </table>
        </div>
        {#if linesNext !== null}<p class="form-actions"><button type="button" class="secondary" onclick={() => loadTrace(linesNext ?? undefined)}>{t('common.loadMore')}</button></p>{/if}
      {/if}
    </section>
  </DomainShell>
</main>
