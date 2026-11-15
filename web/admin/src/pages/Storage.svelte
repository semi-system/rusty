<script lang="ts">
  import { onMount } from 'svelte';
  import type { App } from '../lib/app.svelte';
  import type { Me, Storage } from '../api/types';
  import DomainShell from '../lib/DomainShell.svelte';
  import ErrorNote from '../lib/ErrorNote.svelte';
  import StoragePool from './StoragePool.svelte';
  import { t, tFor } from '../i18n';

  let { app, me, name }: { app: App; me: Me; name: string } = $props();

  // Only a platform administrator may set the store; a domain administrator sees the status.
  const platform = $derived(me.scope === 'platform');

  let storage = $state<Storage | null>(null);
  let loaded = $state(false);
  let error = $state<unknown>(null);
  let notice = $state<string | null>(null);
  let busy = $state(false);
  // A platform administrator who chose a pool for a domain that has no store of its own yet.
  let poolMode = $state(false);

  let endpoint = $state('');
  let region = $state('');
  let bucket = $state('');
  let prefix = $state('');
  let pathStyle = $state(false);
  let accessKey = $state('');
  let secretKey = $state('');

  async function load(): Promise<void> {
    try {
      storage = await app.client.storage(name);
      const s = storage?.settings ?? null;
      if (s !== null) {
        endpoint = s.endpoint;
        region = s.region;
        bucket = s.bucket;
        prefix = s.prefix;
        pathStyle = s.path_style;
      }
      error = null;
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

  const save = (e: SubmitEvent): Promise<void> => {
    e.preventDefault();
    return guarded(async () => {
      storage = await app.client.putStorage(name, { endpoint, region, bucket, prefix, path_style: pathStyle, access_key: accessKey, secret_key: secretKey });
      // The keys are never kept on the page once they have been sent.
      accessKey = '';
      secretKey = '';
      notice = t('storage.saved');
    });
  };

  const check = (): Promise<void> =>
    guarded(async () => {
      await app.client.checkStorage(name);
      notice = t('storage.checkQueued');
    });

  const when = (s: number): string => new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'short' }).format(new Date(s * 1000));
</script>

<main id="content" tabindex="-1">
  <DomainShell {name}>
    <h2>{t('storage.title', { domain: name })}</h2>
    <p class="muted">{t('storage.intro')}</p>
    {#if !loaded}
      <p class="muted" role="status">{t('app.loading')}</p>
    {:else if storage === null}
      <p class="muted">{t('storage.none')}</p>
    {:else}
      <p>{storage.ready ? t('storage.ready') : t('storage.notReady')}</p>
      <p>
        {t('storage.lastCheck')}:
        {#if storage.check_status === 'unchecked' || storage.checked_at === null}
          {t('storage.neverChecked')}
        {:else}
          <span>{tFor('storage.check.', storage.check_status, 'storage.check.unknown')}</span> <span class="muted">({when(storage.checked_at)})</span>
        {/if}
      </p>
      {#if storage.access_key_set !== null && storage.secret_key_set !== null}
        <p>{storage.access_key_set && storage.secret_key_set ? t('storage.keysSet') : t('storage.keysNotSet')}</p>
      {/if}
    {/if}
    {#if notice !== null}<p role="status">{notice}</p>{/if}
    {#if error !== null}<ErrorNote {error} />{/if}

    {#if (storage !== null && storage.kind === 'pool') || (platform && poolMode)}
      <StoragePool {app} {name} {storage} {platform} onupdate={(s) => (storage = s)} />
      {#if platform && storage !== null}
        <p>
          <button type="button" onclick={check} disabled={busy}>{t('storage.check')}</button>
          <button type="button" onclick={load}>{t('dns.reload')}</button>
        </p>
      {:else if !platform}
        <p class="muted">{t('storage.platformOnly')}</p>
        <p><button type="button" onclick={load}>{t('dns.reload')}</button></p>
      {/if}
    {:else if platform}
      <form onsubmit={save}>
        <label>
          {t('storage.endpoint')}
          <input bind:value={endpoint} name="endpoint" type="text" autocapitalize="none" spellcheck="false" required />
        </label>
        <label>
          {t('storage.region')}
          <input bind:value={region} name="region" type="text" autocapitalize="none" spellcheck="false" required />
        </label>
        <label>
          {t('storage.bucket')}
          <input bind:value={bucket} name="bucket" type="text" autocapitalize="none" spellcheck="false" required />
        </label>
        <label>
          {t('storage.prefix')}
          <input bind:value={prefix} name="prefix" type="text" autocapitalize="none" spellcheck="false" />
        </label>
        <label>
          <input bind:checked={pathStyle} name="path_style" type="checkbox" />
          {t('storage.pathStyle')}
        </label>
        <label>
          {t('storage.accessKey')}
          <input bind:value={accessKey} name="access_key" type="text" autocomplete="off" autocapitalize="none" spellcheck="false" required />
        </label>
        <label>
          {t('storage.secretKey')}
          <input bind:value={secretKey} name="secret_key" type="password" autocomplete="new-password" required />
        </label>
        <button type="submit" disabled={busy}>{busy ? t('common.saving') : t('storage.save')}</button>
      </form>
      {#if storage !== null && storage.kind === 's3'}
        <p>
          <button type="button" onclick={check} disabled={busy}>{t('storage.check')}</button>
          <button type="button" onclick={load}>{t('dns.reload')}</button>
        </p>
      {/if}
      {#if storage === null && loaded}
        <p>
          <button type="button" onclick={() => (poolMode = true)}>{t('pool.use')}</button>
        </p>
      {/if}
    {:else if loaded}
      <p class="muted">{t('storage.platformOnly')}</p>
      <p><button type="button" onclick={load}>{t('dns.reload')}</button></p>
    {/if}
  </DomainShell>
</main>
