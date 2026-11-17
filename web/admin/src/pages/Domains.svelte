<script lang="ts">
  import { onMount } from 'svelte';
  import type { App } from '../lib/app.svelte';
  import type { Domain, Me } from '../api/types';
  import Badge from '../lib/Badge.svelte';
  import ErrorNote from '../lib/ErrorNote.svelte';
  import ListBar from '../lib/ListBar.svelte';
  import Link from '../lib/Link.svelte';
  import PageHead from '../lib/PageHead.svelte';
  import { href } from '../lib/router';
  import { labelled } from '../lib/stack';
  import { t, tFor } from '../i18n';

  let { app, me }: { app: App; me: Me } = $props();

  let items = $state<Domain[]>([]);
  let next = $state<number | null>(null);
  let loaded = $state(false);
  let q = $state('');
  let status = $state('');
  let limit = $state(100);
  let total = $state<number | null>(null);
  const narrowed = $derived(q.trim() !== '' || status !== '');
  let error = $state<unknown>(null);
  let name = $state('');
  let busy = $state(false);
  let created = $state<string | null>(null);

  async function load(after?: number, quiet = false): Promise<void> {
    try {
      const page = await app.client.domains(after, { q, status, limit });
      total = page.total;
      items = after === undefined ? page.items : [...items, ...page.items];
      next = page.next;
      error = null;
    } catch (e) {
      if (!quiet) error = e;
    }
    loaded = true;
  }

  onMount(() => {
    void load();
    // A hint refreshes the first page in the background: nothing is shown when that fails (the next hint tries again).
    return app.live.watch(['domains'], () => void load(undefined, true));
  });

  async function create(e: SubmitEvent): Promise<void> {
    e.preventDefault();
    busy = true;
    error = null;
    created = null;
    try {
      const d = await app.client.createDomain(name);
      created = d.name;
      name = '';
      await load();
    } catch (err) {
      error = err;
    } finally {
      busy = false;
    }
  }
</script>

<main id="content" tabindex="-1">
  <PageHead title={t('domains.title')} lede={t('domains.lede')} />

  {#if error !== null}<ErrorNote {error} />{/if}
  {#if created !== null}<p class="alert ok" role="status">{t('domains.created', { name: created })}</p>{/if}

  {#if loaded}
    <ListBar bind:q bind:status bind:limit {total} shown={items.length} label={t('list.searchDomains')} onchange={() => void load()}
      statuses={[{ value: 'active', label: t('domain.status.active') }, { value: 'disabled', label: t('domain.status.disabled') }]} />
  {/if}
  {#if !loaded}
    <p class="muted" role="status">{t('app.loading')}</p>
  {:else if items.length === 0 && error === null}
    <div class="empty"><p>{narrowed ? t('list.noMatch') : t('domains.empty')}</p></div>
  {:else}
    <div class="table-wrap">
      <table use:labelled aria-label={t('domains.list')}>
        <thead>
          <tr><th scope="col">{t('domains.name')}</th><th scope="col">{t('domains.status')}</th></tr>
        </thead>
        <tbody>
          {#each items as d (d.id)}
            <tr>
              <th scope="row"><Link href={href('/domains/{name}/overview', { name: d.name })}>{d.name}</Link></th>
              <td><Badge tone={d.status === 'active' ? 'ok' : 'warn'}>{tFor('domain.status.', d.status, 'domain.status.active')}</Badge></td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
    {#if next !== null}
      <p class="form-actions"><button type="button" class="secondary" onclick={() => load(next ?? undefined)}>{t('common.loadMore')}</button></p>
    {/if}
  {/if}

  {#if me.scope === 'platform'}
    <section class="panel" aria-labelledby="new-domain">
      <h2 id="new-domain">{t('domains.new')}</h2>
      <p class="hint">{t('domains.newLede')}</p>
      <form class="field-row" onsubmit={create}>
        <label>
          {t('domains.newName')}
          <input bind:value={name} name="name" type="text" autocapitalize="none" spellcheck="false" required />
        </label>
        <button type="submit" disabled={busy}>{busy ? t('common.creating') : t('common.create')}</button>
      </form>
    </section>
  {/if}
</main>
