<script lang="ts">
  import { onMount } from 'svelte';
  import type { App } from '../lib/app.svelte';
  import type { Domain, Me } from '../api/types';
  import ErrorNote from '../lib/ErrorNote.svelte';
  import Link from '../lib/Link.svelte';
  import { href } from '../lib/router';
  import { t, tFor } from '../i18n';

  let { app, me }: { app: App; me: Me } = $props();

  let items = $state<Domain[]>([]);
  let next = $state<number | null>(null);
  let loaded = $state(false);
  let error = $state<unknown>(null);
  let name = $state('');
  let busy = $state(false);
  let created = $state<string | null>(null);

  async function load(after?: number): Promise<void> {
    try {
      const page = await app.client.domains(after);
      items = after === undefined ? page.items : [...items, ...page.items];
      next = page.next;
      error = null;
    } catch (e) {
      error = e;
    }
    loaded = true;
  }

  onMount(() => {
    void load();
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
  <h1>{t('domains.title')}</h1>
  {#if !loaded}
    <p class="muted" role="status">{t('app.loading')}</p>
  {:else if items.length === 0 && error === null}
    <p class="muted">{t('domains.empty')}</p>
  {:else}
    <table aria-label={t('domains.list')}>
      <thead>
        <tr><th scope="col">{t('domains.name')}</th><th scope="col">{t('domains.status')}</th></tr>
      </thead>
      <tbody>
        {#each items as d (d.id)}
          <tr>
            <th scope="row"><Link href={href('/domains/{name}/overview', { name: d.name })}>{d.name}</Link></th>
            <td>{tFor('domain.status.', d.status, 'domain.status.active')}</td>
          </tr>
        {/each}
      </tbody>
    </table>
    {#if next !== null}
      <p><button type="button" onclick={() => load(next ?? undefined)}>{t('common.loadMore')}</button></p>
    {/if}
  {/if}
  {#if error !== null}<ErrorNote {error} />{/if}
  {#if created !== null}<p role="status">{t('domains.created', { name: created })}</p>{/if}

  {#if me.scope === 'platform'}
    <h2>{t('domains.new')}</h2>
    <form onsubmit={create}>
      <label>
        {t('domains.newName')}
        <input bind:value={name} name="name" type="text" autocapitalize="none" spellcheck="false" required />
      </label>
      <button type="submit" disabled={busy}>{busy ? t('common.creating') : t('common.create')}</button>
    </form>
  {/if}
</main>
