<script lang="ts">
  import Badge from '../lib/Badge.svelte';
  import { untrack } from 'svelte';
  import type { App } from '../lib/app.svelte';
  import type { Storage, StorageBackend } from '../api/types';
  import ErrorNote from '../lib/ErrorNote.svelte';
  import { labelled } from '../lib/stack';
  import { t, tFor } from '../i18n';

  let {
    app,
    name,
    storage,
    platform,
    onupdate,
  }: { app: App; name: string; storage: Storage | null; platform: boolean; onupdate: (s: Storage | null) => void } = $props();

  let error = $state<unknown>(null);
  let notice = $state<string | null>(null);
  let busy = $state(false);

  // The policy form starts from the pool's policy (or JBOD).
  const start = untrack(() => storage?.policy ?? null);
  let copies = $state(start?.copies ?? 1);
  let syncCopies = $state(start?.sync_copies ?? 1);
  let placement = $state<'ordered' | 'weighted'>(start?.placement === 'ordered' ? 'ordered' : 'weighted');
  let read = $state<'located' | 'ordered' | 'round_robin' | 'fastest'>((['located', 'ordered', 'round_robin', 'fastest'] as const).find((r) => r === start?.read) ?? 'located');

  // A new backend.
  let bname = $state('');
  let endpoint = $state('');
  let region = $state('');
  let bucket = $state('');
  let prefix = $state('');
  let pathStyle = $state(false);
  let weight = $state(100);
  let accessKey = $state('');
  let secretKey = $state('');

  // Edits of existing backends, by name.
  let states = $state<Record<string, string>>({});
  let weights = $state<Record<string, number>>({});
  let newAccess = $state<Record<string, string>>({});
  let newSecret = $state<Record<string, string>>({});

  const backends = $derived<StorageBackend[]>(storage?.backends ?? []);

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

  const savePolicy = (e: SubmitEvent): Promise<void> => {
    e.preventDefault();
    return guarded(async () => {
      onupdate(await app.client.putPool(name, { copies, sync_copies: syncCopies, placement, read }));
      notice = t('pool.saved');
    });
  };

  const addBackend = (e: SubmitEvent): Promise<void> => {
    e.preventDefault();
    return guarded(async () => {
      onupdate(
        await app.client.addBackend(name, { name: bname, endpoint, region, bucket, prefix, path_style: pathStyle, weight, access_key: accessKey, secret_key: secretKey }),
      );
      // The keys are never kept on the page once they have been sent.
      accessKey = '';
      secretKey = '';
      bname = '';
      notice = t('pool.backendAdded');
    });
  };

  const update = (b: StorageBackend): Promise<void> =>
    guarded(async () => {
      onupdate(await app.client.putBackend(name, b.name, { state: states[b.name] ?? b.state, weight: weights[b.name] ?? b.weight }));
      notice = t('pool.backendUpdated');
    });

  const rekey = (b: StorageBackend): Promise<void> =>
    guarded(async () => {
      onupdate(await app.client.backendKeys(name, b.name, { access_key: newAccess[b.name] ?? '', secret_key: newSecret[b.name] ?? '' }));
      newAccess[b.name] = '';
      newSecret[b.name] = '';
      notice = t('pool.keysReplaced');
    });

  const remove = (b: StorageBackend): Promise<void> =>
    guarded(async () => {
      await app.client.deleteBackend(name, b.name);
      onupdate(await app.client.storage(name));
      notice = t('pool.backendRemoved');
    });

  const when = (s: number): string => new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'short' }).format(new Date(s * 1000));
</script>

{#if storage !== null && storage.kind === 'pool'}
  <section aria-labelledby="pool-backends">
    <div class="section-head"><h3 id="pool-backends">{t('pool.backends')}</h3></div>
    {#if backends.length === 0}
      <div class="empty"><p>{t('pool.noBackends')}</p></div>
    {:else}
      <div class="table-wrap">
        <table use:labelled>
          <thead>
            <tr>
              <th>{t('pool.name')}</th>
              <th>{t('pool.state')}</th>
              <th>{t('pool.weight')}</th>
              <th>{t('storage.lastCheck')}</th>
              {#if platform}<th>{t('pool.objects')}</th>{/if}
            </tr>
          </thead>
          <tbody>
            {#each backends as b (b.name)}
              <tr>
                <td>{b.name}{#if b.settings !== null}<br /><span class="muted mono">{b.settings.bucket} · {b.settings.endpoint}</span>{/if}</td>
                <td><Badge tone={b.state === 'active' ? 'ok' : b.state === 'disabled' ? 'danger' : 'warn'}>{tFor('pool.state.', b.state, 'pool.state.unknown')}</Badge></td>
                <td>{b.weight}</td>
                <td>
                  {#if b.check_status === 'unchecked' || b.checked_at === null}
                    <Badge>{t('storage.neverChecked')}</Badge>
                  {:else}
                    <Badge tone={b.check_status === 'ok' ? 'ok' : 'danger'}>{tFor('storage.check.', b.check_status, 'storage.check.unknown')}</Badge> <span class="muted">({when(b.checked_at)})</span>
                  {/if}
                </td>
                {#if platform}<td>{b.objects ?? 0}</td>{/if}
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    {/if}
  </section>
{/if}

{#if notice !== null}<p class="alert ok" role="status">{notice}</p>{/if}
{#if error !== null}<ErrorNote {error} />{/if}

{#if platform}
  <section class="panel">
    <form onsubmit={savePolicy}>
      <div>
        <h3>{t('pool.policy')}</h3>
        <p class="hint">{t('pool.policyHelp')}</p>
      </div>
      <div class="form-grid">
        <label>
          {t('pool.copies')}
          <input bind:value={copies} name="copies" type="number" min="1" max="8" required />
        </label>
        <label>
          {t('pool.syncCopies')}
          <input bind:value={syncCopies} name="sync_copies" type="number" min="1" max="8" required />
        </label>
        <label>
          {t('pool.placement')}
          <select bind:value={placement} name="placement">
            <option value="weighted">{t('pool.placement.weighted')}</option>
            <option value="ordered">{t('pool.placement.ordered')}</option>
          </select>
        </label>
        <label>
          {t('pool.read')}
          <select bind:value={read} name="read">
            <option value="located">{t('pool.read.located')}</option>
            <option value="ordered">{t('pool.read.ordered')}</option>
            <option value="round_robin">{t('pool.read.round_robin')}</option>
            <option value="fastest">{t('pool.read.fastest')}</option>
          </select>
        </label>
      </div>
      <div class="form-actions"><button type="submit" disabled={busy}>{busy ? t('common.saving') : t('pool.save')}</button></div>
    </form>
  </section>

  {#if storage !== null && storage.kind === 'pool'}
    {#each backends as b (b.name)}
      <fieldset class="panel">
        <legend>{b.name}</legend>
        <div class="form-grid">
          <label>
            {t('pool.state')}
            <select value={states[b.name] ?? b.state} onchange={(e) => (states[b.name] = e.currentTarget.value)}>
              <option value="active">{t('pool.state.active')}</option>
              <option value="readonly">{t('pool.state.readonly')}</option>
              <option value="draining">{t('pool.state.draining')}</option>
              <option value="disabled">{t('pool.state.disabled')}</option>
            </select>
          </label>
          <label>
            {t('pool.weight')}
            <input value={weights[b.name] ?? b.weight} type="number" min="1" max="1000" oninput={(e) => (weights[b.name] = Number(e.currentTarget.value))} />
          </label>
        </div>
        <div class="form-actions"><button type="button" onclick={() => update(b)} disabled={busy}>{t('pool.update')}</button></div>
        <div class="form-grid">
          <label>
            {t('storage.accessKey')}
            <input value={newAccess[b.name] ?? ''} type="text" autocomplete="off" autocapitalize="none" spellcheck="false" oninput={(e) => (newAccess[b.name] = e.currentTarget.value)} />
          </label>
          <label>
            {t('storage.secretKey')}
            <input value={newSecret[b.name] ?? ''} type="password" autocomplete="new-password" oninput={(e) => (newSecret[b.name] = e.currentTarget.value)} />
          </label>
        </div>
        <div class="form-actions">
          <button type="button" onclick={() => rekey(b)} disabled={busy}>{t('pool.rekey')}</button>
          <button type="button" class="danger" onclick={() => remove(b)} disabled={busy}>{t('pool.remove')}</button>
        </div>
      </fieldset>
    {/each}

    <section class="panel">
      <form onsubmit={addBackend}>
        <h3>{t('pool.addBackend')}</h3>
        <div class="form-grid">
          <label>
            {t('pool.name')}
            <input bind:value={bname} name="name" type="text" autocapitalize="none" spellcheck="false" required />
          </label>
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
            {t('pool.weight')}
            <input bind:value={weight} name="weight" type="number" min="1" max="1000" required />
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
        <div class="form-actions"><button type="submit" disabled={busy}>{busy ? t('common.saving') : t('pool.addBackendSave')}</button></div>
      </form>
    </section>
  {/if}
{/if}
