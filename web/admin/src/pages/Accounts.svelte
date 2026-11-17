<script lang="ts">
  import Badge from '../lib/Badge.svelte';
  import { onMount } from 'svelte';
  import type { App } from '../lib/app.svelte';
  import type { Account } from '../api/types';
  import DomainShell from '../lib/DomainShell.svelte';
  import ErrorNote from '../lib/ErrorNote.svelte';
  import { labelled } from '../lib/stack';
  import { t, tFor } from '../i18n';

  const MIN = 8;
  let { app, name }: { app: App; name: string } = $props();

  let items = $state<Account[]>([]);
  let next = $state<number | null>(null);
  let loaded = $state(false);
  let error = $state<unknown>(null);
  let notice = $state<string | null>(null);
  let busy = $state(false);

  let local = $state('');
  let password = $state('');
  let tooShort = $state(false);

  let editing = $state<string | null>(null);
  let fresh = $state('');

  async function load(after?: number): Promise<void> {
    try {
      const page = await app.client.accounts(name, after);
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
    tooShort = false;
    if (password !== '' && [...password].length < MIN) {
      tooShort = true;
      return Promise.resolve();
    }
    return guarded(async () => {
      const made = await app.client.createAccount(name, local, password);
      notice = t('accounts.created', { address: made.address }) + (made.aliases_created.length > 0 ? ' ' + t('accounts.rolesCreated', { list: made.aliases_created.join(', ') }) : '');
      local = '';
      password = '';
      await load();
    });
  };

  const toggle = (a: Account): Promise<void> =>
    guarded(async () => {
      const updated = await app.client.setAccountEnabled(a.address, a.status !== 'active');
      items = items.map((x) => (x.id === updated.id ? updated : x));
    });

  const changePassword = (e: SubmitEvent, address: string): Promise<void> => {
    e.preventDefault();
    tooShort = false;
    if ([...fresh].length < MIN) {
      tooShort = true;
      return Promise.resolve();
    }
    return guarded(async () => {
      await app.client.setPassword(address, fresh);
      notice = t('accounts.passwordSet', { address });
      editing = null;
      fresh = '';
      await load();
    });
  };
</script>

<main id="content" tabindex="-1">
  <DomainShell {name}>
    <div class="section-head">
      <h2>{t('accounts.title', { domain: name })}</h2>
      <p>{t('accounts.lede')}</p>
    </div>
    {#if error !== null}<ErrorNote {error} />{/if}
    {#if tooShort}<p class="alert danger" role="alert">{t('accounts.tooShort', { min: MIN })}</p>{/if}
    {#if notice !== null}<p class="alert ok" role="status">{notice}</p>{/if}

    {#if !loaded}
      <p class="muted" role="status">{t('app.loading')}</p>
    {:else if items.length === 0}
      <div class="empty"><p>{t('accounts.empty')}</p></div>
    {:else}
      <div class="table-wrap">
        <table use:labelled aria-label={t('accounts.list')}>
          <thead>
            <tr>
              <th scope="col">{t('accounts.address')}</th>
              <th scope="col">{t('accounts.status')}</th>
              <th scope="col">{t('accounts.password')}</th>
              <th scope="col" class="col-actions"><span class="sr">{t('domain.accounts')}</span></th>
            </tr>
          </thead>
          <tbody>
            {#each items as a (a.id)}
              <tr>
                <th scope="row">{a.address}</th>
                <td><Badge tone={a.status === 'active' ? 'ok' : 'warn'}>{tFor('accounts.status.', a.status, 'accounts.status.active')}</Badge></td>
                <td>{a.has_password ? t('accounts.hasPassword') : t('accounts.noPassword')}</td>
                <td class="col-actions">
                  <div class="row-actions">
                    <button type="button" disabled={busy} aria-label={a.status === 'active' ? t('accounts.disable', { address: a.address }) : t('accounts.enable', { address: a.address })} onclick={() => toggle(a)}>{a.status === 'active' ? t('accounts.disableShort') : t('accounts.enableShort')}</button>
                    <button type="button" aria-label={t('accounts.setPassword', { address: a.address })} onclick={() => { editing = editing === a.address ? null : a.address; fresh = ''; tooShort = false; }}>{t('accounts.setPasswordShort')}</button>
                  </div>
                  {#if editing === a.address}
                    <form class="row-form" onsubmit={(e) => changePassword(e, a.address)}>
                      <label>
                        {t('accounts.passwordFor', { address: a.address })}
                        <input bind:value={fresh} name="password" type="password" autocomplete="new-password" required />
                      </label>
                      <div class="form-actions"><button type="submit" class="primary" disabled={busy}>{busy ? t('common.saving') : t('common.save')}</button></div>
                    </form>
                  {/if}
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
      {#if next !== null}<p class="form-actions"><button type="button" class="secondary" onclick={() => load(next ?? undefined)}>{t('common.loadMore')}</button></p>{/if}
    {/if}

    <section class="panel" aria-labelledby="new-account">
      <h2 id="new-account">{t('accounts.new')}</h2>
      <form onsubmit={create}>
        <div class="field-row">
          <label>
            {t('accounts.local')}
            <input bind:value={local} name="local" type="text" autocapitalize="none" spellcheck="false" required />
          </label>
          <label>
            {t('accounts.newPassword', { min: MIN })}
            <input bind:value={password} name="newpassword" type="password" autocomplete="new-password" />
          </label>
          <button type="submit" disabled={busy}>{busy ? t('common.creating') : t('common.create')}</button>
        </div>
      </form>
    </section>
  </DomainShell>
</main>
