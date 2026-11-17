<script lang="ts">
  import Badge from '../lib/Badge.svelte';
  import { onMount } from 'svelte';
  import type { App } from '../lib/app.svelte';
  import type { Account, Plan } from '../api/types';
  import { bytes } from '../lib/bytes';
  import DomainShell from '../lib/DomainShell.svelte';
  import ErrorNote from '../lib/ErrorNote.svelte';
  import { labelled } from '../lib/stack';
  import ListBar from '../lib/ListBar.svelte';
  import { t, tFor } from '../i18n';

  const MIN = 8;
  const when = (s: number): string => new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'short' }).format(new Date(s * 1000));
  let { app, name }: { app: App; name: string } = $props();

  let items = $state<Account[]>([]);
  let next = $state<number | null>(null);
  let loaded = $state(false);
  let error = $state<unknown>(null);
  let notice = $state<string | null>(null);
  let q = $state('');
  let status = $state('');
  let limit = $state(100);
  let total = $state<number | null>(null);
  const narrowed = $derived(q.trim() !== '' || status !== '');
  let busy = $state(false);

  let local = $state('');
  let password = $state('');
  let tooShort = $state(false);

  let editing = $state<string | null>(null);
  let fresh = $state('');

  async function load(after?: number, quiet = false): Promise<void> {
    try {
      const page = await app.client.accounts(name, after, { q, status, limit });
      total = page.total;
      items = after === undefined ? page.items : [...items, ...page.items];
      next = page.next;
    } catch (e) {
      if (!quiet) error = e;
    }
    loaded = true;
  }

  // The plans an account can be put on; when they cannot be read the column shows the plan name and offers no choice.
  let plans = $state<Plan[]>([]);
  const setPlan = (a: Account, plan: string): Promise<void> =>
    guarded(async () => {
      await app.client.setAccountPlan(a.address, plan === '' ? null : plan);
      await load(undefined, true);
    });

  /** What the account holds and may hold: `12 MB of 1 GB`, or just what it holds without a quota. */
  const storage = (a: Account): string => (a.quota_bytes === null ? bytes(a.used_bytes) : t('accounts.storageOf', { used: bytes(a.used_bytes), quota: bytes(a.quota_bytes) }));
  const nearFull = (a: Account): boolean => a.quota_bytes !== null && a.used_bytes >= a.quota_bytes * 0.9;

  onMount(() => {
    void app.client.plans().then((p) => (plans = p), () => undefined);
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

  const change = (a: Account, action: 'enable' | 'lock' | 'disable'): Promise<void> =>
    guarded(async () => {
      const updated = await app.client.setAccountStatus(a.address, action);
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

    {#if loaded}
      <ListBar bind:q bind:status bind:limit {total} shown={items.length} label={t('list.searchAccounts')} onchange={() => void load()}
        statuses={[{ value: 'active', label: t('accounts.status.active') }, { value: 'locked', label: t('accounts.status.locked') }, { value: 'disabled', label: t('accounts.status.disabled') }]} />
    {/if}
    {#if !loaded}
      <p class="muted" role="status">{t('app.loading')}</p>
    {:else if items.length === 0}
      <div class="empty"><p>{narrowed ? t('list.noMatch') : t('accounts.empty')}</p></div>
    {:else}
      <div class="table-wrap">
        <table use:labelled aria-label={t('accounts.list')}>
          <thead>
            <tr>
              <th scope="col">{t('accounts.address')}</th>
              <th scope="col">{t('accounts.status')}</th>
              <th scope="col">{t('accounts.password')}</th>
              <th scope="col">{t('accounts.plan')}</th>
              <th scope="col">{t('accounts.storage')}</th>
              <th scope="col">{t('accounts.lastLogin')}</th>
              <th scope="col" class="col-actions"><span class="sr">{t('domain.accounts')}</span></th>
            </tr>
          </thead>
          <tbody>
            {#each items as a (a.id)}
              <tr>
                <th scope="row">{a.address}</th>
                <td><Badge tone={a.status === 'active' ? 'ok' : 'warn'}>{tFor('accounts.status.', a.status, 'accounts.status.active')}</Badge></td>
                <td>{a.has_password ? t('accounts.hasPassword') : t('accounts.noPassword')}</td>
                <td>
                  {#if plans.length > 0}
                    <select aria-label={t('accounts.planFor', { address: a.address })} disabled={busy} value={a.plan ?? ''} onchange={(e) => setPlan(a, e.currentTarget.value)}>
                      <option value="">{t('accounts.noPlan')}</option>
                      {#each plans as p (p.name)}<option value={p.name}>{p.name}</option>{/each}
                    </select>
                  {:else}{a.plan ?? t('accounts.noPlan')}{/if}
                </td>
                <td>{storage(a)}{#if nearFull(a)} <Badge tone="warn">{t('accounts.nearFull')}</Badge>{/if}</td>
                <td>{a.last_login_at === null ? t('common.never') : when(a.last_login_at)}</td>
                <td class="col-actions">
                  <div class="row-actions">
                    {#if a.status === 'active'}
                      <button type="button" disabled={busy} aria-label={t('accounts.lock', { address: a.address })} onclick={() => change(a, 'lock')}>{t('accounts.lockShort')}</button>
                      <button type="button" disabled={busy} aria-label={t('accounts.disable', { address: a.address })} onclick={() => change(a, 'disable')}>{t('accounts.disableShort')}</button>
                    {:else}
                      <button type="button" disabled={busy} aria-label={t('accounts.enable', { address: a.address })} onclick={() => change(a, 'enable')}>{t('accounts.enableShort')}</button>
                    {/if}
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
