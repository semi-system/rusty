<script lang="ts">
  import { onMount } from 'svelte';
  import type { App } from '../lib/app.svelte';
  import type { AuditEntry } from '../api/types';
  import ErrorNote from '../lib/ErrorNote.svelte';
  import PageHead from '../lib/PageHead.svelte';
  import { labelled } from '../lib/stack';
  import { t } from '../i18n';

  let { app }: { app: App } = $props();

  const fields = (detail: string | null): [string, string][] => {
    if (detail === null) return [];
    try {
      return Object.entries(JSON.parse(detail) as Record<string, string>);
    } catch {
      return [];
    }
  };

  const when = (s: number): string => new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'medium' }).format(new Date(s * 1000));

  let items = $state<AuditEntry[]>([]);
  let next = $state<number | null>(null);
  let domain = $state('');
  let loaded = $state(false);
  let error = $state<unknown>(null);

  async function load(before?: number): Promise<void> {
    error = null;
    try {
      const page = await app.client.audit({ domain, before });
      items = before === undefined ? page.items : [...items, ...page.items];
      next = page.next;
    } catch (e) {
      error = e;
    }
    loaded = true;
  }

  onMount(() => void load());
</script>

<main id="content" tabindex="-1">
  <PageHead title={t('audit.title')} lede={t('audit.lede')} />
  {#if error !== null}<ErrorNote {error} />{/if}
  <form class="field-row" onsubmit={(e) => { e.preventDefault(); void load(); }}>
    <label>{t('audit.domain')}<input bind:value={domain} name="domain" type="search" autocapitalize="none" spellcheck="false" /></label>
    <button type="submit" class="secondary">{t('audit.find')}</button>
  </form>
  {#if !loaded}
    <p class="muted" role="status">{t('app.loading')}</p>
  {:else if items.length === 0}
    <div class="empty"><p>{t('audit.empty')}</p></div>
  {:else}
    <div class="table-wrap">
      <table use:labelled aria-label={t('audit.title')}>
        <thead>
          <tr>
            <th scope="col">{t('audit.col.when')}</th>
            <th scope="col">{t('audit.col.who')}</th>
            <th scope="col">{t('audit.col.what')}</th>
            <th scope="col">{t('audit.col.from')}</th>
          </tr>
        </thead>
        <tbody>
          {#each items as a (a.id)}
            <tr>
              <td>{when(a.at)}</td>
              <th scope="row">{a.actor}</th>
              <td>
                {a.method} {a.path}
                {#if fields(a.detail).length > 0}
                  <dl class="changes">
                    {#each fields(a.detail) as [name, value] (name)}
                      <div><dt>{name}</dt><dd>{value === '(hidden)' ? t('audit.hidden') : value}</dd></div>
                    {/each}
                  </dl>
                {/if}
              </td>
              <td>{a.ip}</td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
    {#if next !== null}
      <p><button type="button" class="secondary" onclick={() => load(next ?? undefined)}>{t('audit.more')}</button></p>
    {/if}
  {/if}
</main>
