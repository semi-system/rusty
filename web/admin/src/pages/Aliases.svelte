<script lang="ts">
  import { onMount } from 'svelte';
  import type { App } from '../lib/app.svelte';
  import type { Alias } from '../api/types';
  import DomainShell from '../lib/DomainShell.svelte';
  import ErrorNote from '../lib/ErrorNote.svelte';
  import { t } from '../i18n';

  let { app, name }: { app: App; name: string } = $props();

  let items = $state<Alias[]>([]);
  let next = $state<number | null>(null);
  let loaded = $state(false);
  let error = $state<unknown>(null);
  let notice = $state<string | null>(null);
  let busy = $state(false);
  let local = $state('');
  let target = $state('');

  async function load(after?: number): Promise<void> {
    try {
      const page = await app.client.aliases(name, after);
      items = after === undefined ? page.items : [...items, ...page.items];
      next = page.next;
    } catch (e) {
      error = e;
    }
    loaded = true;
  }

  onMount(() => {
    void load();
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
    <h2>{t('aliases.title', { domain: name })}</h2>
    {#if !loaded}
      <p class="muted" role="status">{t('app.loading')}</p>
    {:else if items.length === 0}
      <p class="muted">{t('aliases.empty')}</p>
    {:else}
      <table aria-label={t('aliases.list')}>
        <thead>
          <tr><th scope="col">{t('aliases.alias')}</th><th scope="col">{t('aliases.target')}</th><th scope="col"><span class="sr">{t('domain.aliases')}</span></th></tr>
        </thead>
        <tbody>
          {#each items as a (a.id)}
            <tr>
              <th scope="row">{a.alias}</th>
              <td>{a.target}</td>
              <td><button type="button" disabled={busy} onclick={() => remove(a)}>{t('aliases.remove', { alias: a.alias })}</button></td>
            </tr>
          {/each}
        </tbody>
      </table>
      {#if next !== null}<p><button type="button" onclick={() => load(next ?? undefined)}>{t('common.loadMore')}</button></p>{/if}
    {/if}
    {#if error !== null}<ErrorNote {error} />{/if}
    {#if notice !== null}<p role="status">{notice}</p>{/if}

    <h2>{t('aliases.new')}</h2>
    <form onsubmit={create}>
      <label>
        {t('aliases.local')}
        <input bind:value={local} name="local" type="text" autocapitalize="none" spellcheck="false" required />
      </label>
      <label>
        {t('aliases.targetAddress')}
        <input bind:value={target} name="target" type="text" inputmode="email" autocapitalize="none" spellcheck="false" required />
      </label>
      <button type="submit" disabled={busy}>{busy ? t('common.creating') : t('common.create')}</button>
    </form>
  </DomainShell>
</main>
