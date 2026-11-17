<script lang="ts">
  import Badge from '../lib/Badge.svelte';
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
    <div class="section-head">
      <h2>{t('storage.title', { domain: name })}</h2>
      <p>{t('storage.lede')}</p>
    </div>
    <p class="muted">{t('storage.intro')}</p>
    {#if notice !== null}<p class="alert ok" role="status">{notice}</p>{/if}
    {#if error !== null}<ErrorNote {error} />{/if}

    {#if !loaded}
      <p class="muted" role="status">{t('app.loading')}</p>
    {:else if storage === null}
      <div class="empty"><p>{t('storage.none')}</p></div>
    {:else}
      <section class="panel">
        <ul class="checklist">
          <li><Badge tone={storage.ready ? 'ok' : 'warn'}>{storage.ready ? t('storage.ready') : t('storage.notReady')}</Badge></li>
          <li>
            <span class="muted">{t('storage.lastCheck')}:</span>
            {#if storage.check_status === 'unchecked' || storage.checked_at === null}
              <Badge>{t('storage.neverChecked')}</Badge>
            {:else}
              <Badge tone={storage.check_status === 'ok' ? 'ok' : 'danger'}>{tFor('storage.check.', storage.check_status, 'storage.check.unknown')}</Badge>
              <span class="muted">({when(storage.checked_at)})</span>
            {/if}
          </li>
          {#if storage.access_key_set !== null && storage.secret_key_set !== null}
            <li><Badge tone={storage.access_key_set && storage.secret_key_set ? 'ok' : 'warn'}>{storage.access_key_set && storage.secret_key_set ? t('storage.keysSet') : t('storage.keysNotSet')}</Badge></li>
          {/if}
        </ul>
      </section>
    {/if}

    {#if (storage !== null && storage.kind === 'pool') || (platform && poolMode)}
      <StoragePool {app} {name} {storage} {platform} onupdate={(s) => (storage = s)} />
      {#if platform && storage !== null}
        <div class="form-actions">
          <button type="button" onclick={check} disabled={busy}>{t('storage.check')}</button>
          <button type="button" class="secondary" onclick={load}>{t('dns.reload')}</button>
        </div>
      {:else if !platform}
        <p class="muted">{t('storage.platformOnly')}</p>
        <div class="form-actions"><button type="button" class="secondary" onclick={load}>{t('dns.reload')}</button></div>
      {/if}
    {:else if platform}
      <section class="panel">
        <form onsubmit={save}>
          <div class="form-grid">
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
              {t('storage.accessKey')}
              <input bind:value={accessKey} name="access_key" type="text" autocomplete="off" autocapitalize="none" spellcheck="false" required />
            </label>
            <label>
              {t('storage.secretKey')}
              <input bind:value={secretKey} name="secret_key" type="password" autocomplete="new-password" required />
            </label>
          </div>
          <label class="check">
            <input bind:checked={pathStyle} name="path_style" type="checkbox" />
            {t('storage.pathStyle')}
          </label>
          <div class="form-actions"><button type="submit" disabled={busy}>{busy ? t('common.saving') : t('storage.save')}</button></div>
        </form>
      </section>
      <div class="form-actions">
        {#if storage !== null && storage.kind === 's3'}
          <button type="button" onclick={check} disabled={busy}>{t('storage.check')}</button>
          <button type="button" class="secondary" onclick={load}>{t('dns.reload')}</button>
        {/if}
        {#if storage === null && loaded}
          <button type="button" class="secondary" onclick={() => (poolMode = true)}>{t('pool.use')}</button>
        {/if}
      </div>
    {:else if loaded}
      <p class="muted">{t('storage.platformOnly')}</p>
      <div class="form-actions"><button type="button" class="secondary" onclick={load}>{t('dns.reload')}</button></div>
    {/if}
  </DomainShell>
</main>
