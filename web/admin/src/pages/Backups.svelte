<script lang="ts">
  import { onMount } from 'svelte';
  import type { App } from '../lib/app.svelte';
  import type { BackupRun } from '../api/types';
  import Badge from '../lib/Badge.svelte';
  import ErrorNote from '../lib/ErrorNote.svelte';
  import PageHead from '../lib/PageHead.svelte';
  import { labelled } from '../lib/stack';
  import { t, tFor } from '../i18n';

  let { app }: { app: App } = $props();

  const when = (s: number): string => new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'short' }).format(new Date(s * 1000));
  const mib = (b: number | null): string => (b === null ? '' : (b / 1048576).toFixed(b < 1048576 ? 2 : 1) + ' MiB');

  let items = $state<BackupRun[]>([]);
  let now = $state(0);
  let loaded = $state(false);
  let error = $state<unknown>(null);

  async function load(): Promise<void> {
    error = null;
    try {
      const r = await app.client.backups();
      items = r.items;
      now = r.now;
    } catch (e) {
      error = e;
    }
    loaded = true;
  }
  onMount(() => void load());

  const lastOf = (kind: string, ok: boolean): BackupRun | undefined => items.find((r) => r.kind === kind && r.ok === ok);
  const newest = (kind: string): BackupRun | undefined => items.find((r) => r.kind === kind);
  const age = (r: BackupRun): string => {
    const h = Math.floor((now - r.at) / 3600);
    return h < 48 ? t('backups.hours', { n: h }) : t('backups.days', { n: Math.floor(h / 24) });
  };
  // A nightly backup that is older than a day and a half has missed a night; older than three days is a stopped timer.
  const tone = (r: BackupRun | undefined): 'ok' | 'warn' | 'danger' => (r === undefined ? 'danger' : now - r.at > 3 * 86400 ? 'danger' : now - r.at > 36 * 3600 ? 'warn' : 'ok');

  let lastBackup = $derived(lastOf('backup', true));
  let lastDrill = $derived(newest('verify'));
</script>

<main id="content" tabindex="-1">
  <PageHead title={t('backups.title')} lede={t('backups.lede')} />
  {#if error !== null}<ErrorNote {error} />{/if}
  {#if !loaded}
    <p class="muted" role="status">{t('app.loading')}</p>
  {:else if items.length === 0}
    <div class="empty"><p>{t('backups.empty')}</p></div>
  {:else}
    <section aria-label={t('backups.summary')}>
      <p>
        <Badge tone={tone(lastBackup)}>{lastBackup === undefined ? t('backups.none') : t('backups.last', { when: when(lastBackup.at), age: age(lastBackup) })}</Badge>
        {#if lastBackup?.bytes != null}<span class="muted">{mib(lastBackup.bytes)}</span>{/if}
      </p>
      <p>
        {#if lastDrill === undefined}
          <Badge tone="warn">{t('backups.drill.none')}</Badge>
        {:else}
          <Badge tone={lastDrill.ok ? 'ok' : 'danger'}>{lastDrill.ok ? t('backups.drill.ok', { age: age(lastDrill) }) : t('backups.drill.failed', { age: age(lastDrill) })}</Badge>
          <span class="muted">{lastDrill.detail}</span>
        {/if}
      </p>
    </section>
    <div class="table-wrap">
      <table use:labelled aria-label={t('backups.title')}>
        <thead>
          <tr>
            <th scope="col">{t('backups.col.when')}</th>
            <th scope="col">{t('backups.col.what')}</th>
            <th scope="col">{t('backups.col.result')}</th>
            <th scope="col">{t('backups.col.size')}</th>
            <th scope="col">{t('backups.col.detail')}</th>
          </tr>
        </thead>
        <tbody>
          {#each items as r (r.id)}
            <tr>
              <td>{when(r.at)}</td>
              <th scope="row">{tFor('backups.kind.', r.kind, 'backups.kind.backup')}</th>
              <td><Badge tone={r.ok ? 'ok' : 'danger'}>{r.ok ? t('backups.ok') : t('backups.failed')}</Badge></td>
              <td>{mib(r.bytes)}</td>
              <td>{r.detail}</td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
  {/if}
</main>
