<script lang="ts">
  import { onMount } from 'svelte';
  import type { App } from '../lib/app.svelte';
  import type { Alias } from '../api/types';
  import DomainShell from '../lib/DomainShell.svelte';
  import ErrorNote from '../lib/ErrorNote.svelte';
  import ListBar from '../lib/ListBar.svelte';
  import { labelled } from '../lib/stack';
  import { t } from '../i18n';

  let { app, name }: { app: App; name: string } = $props();

  let items = $state<Alias[]>([]);
  let next = $state<number | null>(null);
  let loaded = $state(false);
  let q = $state('');
  let status = $state('');
  let limit = $state(100);
  let total = $state<number | null>(null);
  const narrowed = $derived(q.trim() !== '' || status !== '');
  let error = $state<unknown>(null);
  let notice = $state<string | null>(null);
  let busy = $state(false);
  let local = $state('');
  let target = $state('');

  async function load(after?: number, quiet = false): Promise<void> {
    try {
      const page = await app.client.aliases(name, after, { q, limit });
      total = page.total;
      items = after === undefined ? page.items : [...items, ...page.items];
      next = page.next;
    } catch (e) {
      if (!quiet) error = e;
    }
    loaded = true;
  }

  onMount(() => {
    void load();
    // A hint refreshes the first page in the background: nothing is shown when that fails (the next hint tries again).
    return app.live.watch(['accounts:' + name.toLowerCase()], () => void load(undefined, true));
  });

  async function guarded(work: () => Promise<void>): Promise<void> {
    busy = true;
    error = null;
    notice = null;
    try {
      await work();
    } catch (e) {
      error = e;
    } finally {
      busy = false;
    }
  }

  const create = (e: SubmitEvent): Promise<void> => {
    e.preventDefault();
    return guarded(async () => {
      await app.client.createAlias(name, local, target);
      notice = t('aliases.created', { alias: `${local}@${name}` });
      local = '';
      target = '';
      await load();
    });
  };

  const remove = (a: Alias): Promise<void> =>
    guarded(async () => {
      await app.client.deleteAlias(a.alias);
      items = items.filter((x) => x.id !== a.id);
    });
</script>

<main id="content" tabindex="-1">
  <DomainShell {name}>
    <div class="section-head">
      <h2>{t('aliases.title', { domain: name })}</h2>
      <p>{t('aliases.lede')}</p>
    </div>
    {#if error !== null}<ErrorNote {error} />{/if}
    {#if notice !== null}<p class="alert ok" role="status">{notice}</p>{/if}

    {#if loaded}
      <ListBar bind:q bind:status bind:limit {total} shown={items.length} label={t('list.searchAliases')} onchange={() => void load()} />
    {/if}
    {#if !loaded}
      <p class="muted" role="status">{t('app.loading')}</p>
    {:else if items.length === 0}
      <div class="empty"><p>{narrowed ? t('list.noMatch') : t('aliases.empty')}</p></div>
    {:else}
      <div class="table-wrap">
        <table use:labelled aria-label={t('aliases.list')}>
          <thead>
            <tr><th scope="col">{t('aliases.alias')}</th><th scope="col">{t('aliases.target')}</th><th scope="col" class="col-actions"><span class="sr">{t('domain.aliases')}</span></th></tr>
          </thead>
          <tbody>
            {#each items as a (a.id)}
              <tr>
                <th scope="row">{a.alias}</th>
                <td>{a.target}</td>
                <td class="col-actions"><button type="button" class="danger" disabled={busy} aria-label={t('aliases.remove', { alias: a.alias })} onclick={() => remove(a)}>{t('aliases.removeShort')}</button></td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
      {#if next !== null}<p class="form-actions"><button type="button" class="secondary" onclick={() => load(next ?? undefined)}>{t('common.loadMore')}</button></p>{/if}
    {/if}

    <section class="panel" aria-labelledby="new-alias">
      <h2 id="new-alias">{t('aliases.new')}</h2>
      <form onsubmit={create}>
        <div class="field-row">
          <label>
            {t('aliases.local')}
            <input bind:value={local} name="local" type="text" autocapitalize="none" spellcheck="false" required />
          </label>
          <label>
            {t('aliases.targetAddress')}
            <input bind:value={target} name="target" type="text" inputmode="email" autocapitalize="none" spellcheck="false" required />
          </label>
          <button type="submit" disabled={busy}>{busy ? t('common.creating') : t('common.create')}</button>
        </div>
      </form>
    </section>
  </DomainShell>
</main>
