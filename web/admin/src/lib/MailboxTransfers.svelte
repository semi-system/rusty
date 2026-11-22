<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import type { App } from '../lib/app.svelte';
  import type { Transfer } from '../api/types';
  import { bytes } from './bytes';
  import ErrorNote from './ErrorNote.svelte';
  import { t, tFor } from '../i18n';

  let { app, address, onclose }: { app: App; address: string; onclose: () => void } = $props();

  /** The largest piece of an upload: the server takes at most 48 KiB in one request. */
  const PIECE = 48 * 1024;
  /** The most an import may be (the server refuses more). */
  const MAX_IMPORT = 64 * 1024 * 1024;

  let items = $state<Transfer[]>([]);
  let loaded = $state(false);
  let error = $state<unknown>(null);
  let notice = $state<string | null>(null);
  let busy = $state(false);
  let folder = $state('');
  let progress = $state<string | null>(null);
  let timer: ReturnType<typeof setInterval> | null = null;

  const when = (s: number): string => new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'short' }).format(new Date(s * 1000));
  const waiting = (x: Transfer[]): boolean => x.some((i) => i.state === 'queued' || i.state === 'running' || i.state === 'uploading');

  async function load(quiet = false): Promise<void> {
    try {
      items = (await app.client.transfers(address)).items;
    } catch (e) {
      if (!quiet) error = e;
    }
    loaded = true;
    // While something is being done the list refreshes by itself.
    if (waiting(items) && timer === null) timer = setInterval(() => void load(true), 2000);
    if (!waiting(items) && timer !== null) {
      clearInterval(timer);
      timer = null;
    }
  }

  onMount(() => void load());
  onDestroy(() => {
    if (timer !== null) clearInterval(timer);
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
      progress = null;
    }
  }

  const exportMail = (): Promise<void> =>
    guarded(async () => {
      await app.client.startExport(address);
      notice = t('transfers.exportAsked');
      await load(true);
    });

  /** The bytes of a slice of a file. */
  function read(blob: Blob): Promise<Uint8Array> {
    return new Promise((resolve, reject) => {
      const r = new FileReader();
      r.onload = () => resolve(new Uint8Array(r.result as ArrayBuffer));
      r.onerror = () => reject(new TypeError('read'));
      r.readAsArrayBuffer(blob);
    });
  }

  const importFile = (e: Event): Promise<void> => {
    const input = e.currentTarget as HTMLInputElement;
    const file = input.files?.[0];
    input.value = '';
    if (file === undefined) return Promise.resolve();
    return guarded(async () => {
      if (file.size > MAX_IMPORT) {
        notice = t('transfers.tooLarge', { max: bytes(MAX_IMPORT) });
        return;
      }
      const started = await app.client.startImport(address, folder.trim() === '' ? null : folder.trim());
      const pieces = Math.max(1, Math.ceil(file.size / PIECE));
      for (let n = 0; n < pieces; n += 1) {
        progress = t('transfers.uploading', { done: n + 1, total: pieces });
        await app.client.putPiece(started.id, n, await read(file.slice(n * PIECE, (n + 1) * PIECE)));
      }
      await app.client.startTransfer(started.id, pieces);
      notice = t('transfers.importAsked');
      await load(true);
    });
  };
</script>

<section class="row-form" aria-label={t('transfers.title', { address })}>
  <h3>{t('transfers.title', { address })}</h3>
  <p class="hint">{t('transfers.lede')}</p>
  {#if error !== null}<ErrorNote {error} />{/if}
  {#if notice !== null}<p class="alert ok" role="status">{notice}</p>{/if}
  {#if progress !== null}<p class="muted" role="status">{progress}</p>{/if}
  <div class="form-actions">
    <button type="button" class="primary" disabled={busy} onclick={exportMail}>{t('transfers.export')}</button>
    <label class="inline">{t('transfers.folder')}<input bind:value={folder} name="folder" type="text" maxlength="200" /></label>
    <label class="inline">{t('transfers.import')}<input type="file" name="mbox" accept=".mbox,text/plain,application/mbox" disabled={busy} onchange={importFile} /></label>
    <button type="button" class="secondary" onclick={onclose}>{t('rules.close')}</button>
  </div>
  {#if loaded && items.length === 0}
    <p class="muted">{t('transfers.none')}</p>
  {:else if items.length > 0}
    <ul class="plain" aria-label={t('transfers.list')}>
      {#each items as i (i.id)}
        <li>
          <strong>{tFor('transfers.kind.', i.kind, 'transfers.kind.export')}</strong>
          {when(i.created_at)} · {t('transfers.by', { who: i.requested_by })} ·
          {tFor('transfers.state.', i.state, 'transfers.state.queued')}
          {#if i.state === 'done'} · {t('transfers.counts', { messages: i.messages, skipped: i.skipped, size: bytes(i.size) })}{/if}
          {#if i.error !== null} · {tFor('transfers.error.', i.error, 'transfers.error.failed')}{/if}
          {#if i.kind === 'export' && i.state === 'done'}
            · <a href={`/api/v1/transfers/${i.id}/download`} download>{t('transfers.download')}</a>
          {/if}
          <span class="muted">({t('transfers.expires', { when: when(i.expires_at) })})</span>
        </li>
      {/each}
    </ul>
  {/if}
</section>
