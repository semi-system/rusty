<script lang="ts">
  import { onMount } from 'svelte';
  import type { App } from '../lib/app.svelte';
  import type { Dkim } from '../api/types';
  import Copy from '../lib/Copy.svelte';
  import DomainShell from '../lib/DomainShell.svelte';
  import ErrorNote from '../lib/ErrorNote.svelte';
  import { t } from '../i18n';

  let { app, name }: { app: App; name: string } = $props();

  let items = $state<Dkim[]>([]);
  let loaded = $state(false);
  let error = $state<unknown>(null);
  let notice = $state<string | null>(null);
  let busy = $state(false);
  let selector = $state('');

  async function load(): Promise<void> {
    try {
      items = await app.client.dkim(name);
    } catch (e) {
      error = e;
    }
    loaded = true;
  }

  onMount(() => {
    void load();
  });

  async function make(e: SubmitEvent): Promise<void> {
    e.preventDefault();
    busy = true;
    error = null;
    notice = null;
    try {
      const k = await app.client.createDkim(name, selector);
      notice = t('dkim.created', { selector: k.selector });
      selector = '';
      await load();
    } catch (err) {
      error = err;
    } finally {
      busy = false;
    }
  }
</script>

<main id="content" tabindex="-1">
  <DomainShell {name}>
    <h2>{t('dkim.title', { domain: name })}</h2>
    <p class="muted">{t('dkim.intro')}</p>
    {#if !loaded}
      <p class="muted" role="status">{t('app.loading')}</p>
    {:else if items.length === 0}
      <p class="muted">{t('dkim.empty')}</p>
    {:else}
      <ul aria-label={t('dkim.list')} class="plain">
        {#each items as k (k.selector)}
          <li>
            <h3>{k.selector}</h3>
            <p>{k.sealed ? t('dkim.sealed') : t('dkim.notSealed')}</p>
            <p>{t('dkim.recordName')} <Copy value={k.record_name} label={t('dkim.recordName')} /></p>
            <p>{t('dkim.recordValue')} <Copy value={k.record_value} label={t('dkim.recordValue')} /></p>
          </li>
        {/each}
      </ul>
    {/if}
    {#if error !== null}<ErrorNote {error} />{/if}
    {#if notice !== null}<p role="status">{notice}</p>{/if}

    <h2>{t('dkim.new')}</h2>
    <form onsubmit={make}>
      <label>
        {t('dkim.newSelector')}
        <input bind:value={selector} name="selector" type="text" autocapitalize="none" spellcheck="false" />
      </label>
      <button type="submit" disabled={busy}>{busy ? t('common.creating') : t('dkim.new')}</button>
    </form>
  </DomainShell>
</main>
