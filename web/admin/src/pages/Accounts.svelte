<script lang="ts">
  import { onMount } from 'svelte';
  import type { App } from '../lib/app.svelte';
  import type { Account } from '../api/types';
  import DomainShell from '../lib/DomainShell.svelte';
  import ErrorNote from '../lib/ErrorNote.svelte';
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
    <h2>{t('accounts.title', { domain: name })}</h2>
    {#if !loaded}
      <p class="muted" role="status">{t('app.loading')}</p>
    {:else if items.length === 0}
      <p class="muted">{t('accounts.empty')}</p>
    {:else}
      <table aria-label={t('accounts.list')}>
        <thead>
          <tr>
            <th scope="col">{t('accounts.address')}</th>
            <th scope="col">{t('accounts.status')}</th>
            <th scope="col">{t('accounts.password')}</th>
            <th scope="col"><span class="sr">{t('domain.accounts')}</span></th>
          </tr>
        </thead>
        <tbody>
          {#each items as a (a.id)}
            <tr>
              <th scope="row">{a.address}</th>
              <td>{tFor('accounts.status.', a.status, 'accounts.status.active')}</td>
              <td>{a.has_password ? t('accounts.hasPassword') : t('accounts.noPassword')}</td>
              <td>
                <button type="button" disabled={busy} onclick={() => toggle(a)}>{a.status === 'active' ? t('accounts.disable', { address: a.address }) : t('accounts.enable', { address: a.address })}</button>
                <button type="button" onclick={() => { editing = editing === a.address ? null : a.address; fresh = ''; tooShort = false; }}>{t('accounts.setPassword', { address: a.address })}</button>
                {#if editing === a.address}
                  <form onsubmit={(e) => changePassword(e, a.address)}>
                    <label>
                      {t('accounts.passwordFor', { address: a.address })}
                      <input bind:value={fresh} name="password" type="password" autocomplete="new-password" required />
                    </label>
                    <button type="submit" disabled={busy}>{busy ? t('common.saving') : t('common.save')}</button>
                  </form>
                {/if}
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
      {#if next !== null}<p><button type="button" onclick={() => load(next ?? undefined)}>{t('common.loadMore')}</button></p>{/if}
    {/if}
    {#if error !== null}<ErrorNote {error} />{/if}
    {#if tooShort}<p class="note" role="alert">{t('accounts.tooShort', { min: MIN })}</p>{/if}
    {#if notice !== null}<p role="status">{notice}</p>{/if}

    <h2>{t('accounts.new')}</h2>
    <form onsubmit={create}>
      <label>
        {t('accounts.local')}
        <input bind:value={local} name="local" type="text" autocapitalize="none" spellcheck="false" required />
      </label>
      <label>
        {t('accounts.newPassword', { min: MIN })}
        <input bind:value={password} name="newpassword" type="password" autocomplete="new-password" />
      </label>
      <button type="submit" disabled={busy}>{busy ? t('common.creating') : t('common.create')}</button>
    </form>
  </DomainShell>
</main>
