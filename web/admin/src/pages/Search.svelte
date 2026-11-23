<script lang="ts">
  import { onDestroy } from 'svelte';
  import type { App } from '../lib/app.svelte';
  import type { Search } from '../api/types';
  import { ApiError } from '../api/client';
  import Badge from '../lib/Badge.svelte';
  import ErrorNote from '../lib/ErrorNote.svelte';
  import PageHead from '../lib/PageHead.svelte';
  import { labelled } from '../lib/stack';
  import { bytes } from '../lib/bytes';
  import { t, tFor } from '../i18n';

  let { app }: { app: App } = $props();

  const when = (s: number): string => new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'short' }).format(new Date(s * 1000));
  // A date typed as YYYY-MM-DD is the start (or the end) of that day, UTC.
  const day = (v: string, end: boolean): number | null => (v.trim() === '' ? null : Math.floor(Date.parse(v.trim() + (end ? 'T23:59:59Z' : 'T00:00:00Z')) / 1000));

  let domain = $state('');
  let account = $state('');
  let from = $state('');
  let to = $state('');
  let subject = $state('');
  let since = $state('');
  let until = $state('');
  let busy = $state(false);
  let error = $state<unknown>(null);
  let off = $state(false);
  let invalid = $state(false);
  let result = $state<Search | null>(null);
  let timer: ReturnType<typeof setTimeout> | undefined;
  let stopped = false;

  onDestroy(() => {
    stopped = true;
    clearTimeout(timer);
  });

  async function poll(id: number, tries: number): Promise<void> {
    try {
      result = await app.client.searchResult(id);
    } catch (e) {
      error = e;
      busy = false;
      return;
    }
    if (result.state === 'done' || result.state === 'failed' || tries <= 0 || stopped) {
      busy = false;
      return;
    }
    timer = setTimeout(() => void poll(id, tries - 1), 1500);
  }

  async function ask(e: SubmitEvent): Promise<void> {
    e.preventDefault();
    const sinceTs = day(since, false);
    const untilTs = day(until, true);
    const text = (v: string): string | undefined => (v.trim() === '' ? undefined : v.trim());
    invalid = Number.isNaN(sinceTs) || Number.isNaN(untilTs) || (text(from) === undefined && text(to) === undefined && text(subject) === undefined && sinceTs === null && untilTs === null);
    if (invalid) return;
    busy = true;
    error = null;
    off = false;
    result = null;
    try {
      const q = await app.client.askSearch({ domain: text(domain), account: text(account), from: text(from), to: text(to), subject: text(subject), since: sinceTs ?? undefined, until: untilTs ?? undefined });
      void poll(q.id, 40);
    } catch (err) {
      busy = false;
      if (err instanceof ApiError && err.code === 'wrong_state') off = true;
      else error = err;
    }
  }
</script>

<main id="content" tabindex="-1">
  <PageHead title={t('search.title')} lede={t('search.lede')} />
  {#if error !== null}<ErrorNote {error} />{/if}
  {#if off}<p class="alert danger" role="alert">{t('search.off')}</p>{/if}
  {#if invalid}<p class="alert danger" role="alert">{t('search.invalid')}</p>{/if}
  <form onsubmit={ask}>
    <div class="field-row">
      <label>{t('search.from')}<input bind:value={from} name="from" type="text" autocapitalize="none" spellcheck="false" /></label>
      <label>{t('search.to')}<input bind:value={to} name="to" type="text" autocapitalize="none" spellcheck="false" /></label>
      <label>{t('search.subject')}<input bind:value={subject} name="subject" type="text" spellcheck="false" /></label>
    </div>
    <div class="field-row">
      <label>{t('search.since')}<input bind:value={since} name="since" type="text" placeholder="2026-01-31" inputmode="numeric" /></label>
      <label>{t('search.until')}<input bind:value={until} name="until" type="text" placeholder="2026-12-31" inputmode="numeric" /></label>
      <label>{t('search.domain')}<input bind:value={domain} name="domain" type="text" autocapitalize="none" spellcheck="false" /></label>
      <label>{t('search.account')}<input bind:value={account} name="account" type="text" autocapitalize="none" spellcheck="false" /></label>
      <button type="submit" class="primary" disabled={busy}>{busy ? t('search.running') : t('search.go')}</button>
    </div>
  </form>
  <p class="hint">{t('search.audited')}</p>
  {#if result !== null}
    {#if result.state === 'failed'}
      <p class="alert danger" role="alert">{tFor('search.error.', result.error ?? 'failed', 'search.error.failed')}</p>
    {:else if result.state === 'done'}
      {#if result.results.length === 0}
        <div class="empty"><p>{t('search.none')}</p></div>
      {:else}
        <p><Badge tone={result.truncated ? 'warn' : 'ok'}>{result.truncated ? t('search.truncated', { n: result.found }) : t('search.found', { n: result.found })}</Badge></p>
        <div class="table-wrap">
          <table use:labelled aria-label={t('search.title')}>
            <thead>
              <tr>
                <th scope="col">{t('search.col.when')}</th>
                <th scope="col">{t('search.col.account')}</th>
                <th scope="col">{t('search.col.folder')}</th>
                <th scope="col">{t('search.col.from')}</th>
                <th scope="col">{t('search.col.subject')}</th>
                <th scope="col">{t('search.col.size')}</th>
              </tr>
            </thead>
            <tbody>
              {#each result.results as r, i (i)}
                <tr>
                  <td>{when(r.at)}</td>
                  <th scope="row">{r.account}</th>
                  <td>{r.folder}</td>
                  <td>{r.from}</td>
                  <td>{r.subject}</td>
                  <td>{bytes(r.size)}</td>
                </tr>
              {/each}
            </tbody>
          </table>
        </div>
      {/if}
    {:else}
      <p class="muted" role="status">{t('search.working')}</p>
    {/if}
  {/if}
</main>
