<script lang="ts">
  import Badge from '../lib/Badge.svelte';
  import { onMount } from 'svelte';
  import type { App } from '../lib/app.svelte';
  import type { Account, ImportResult, Me, Plan } from '../api/types';
  import { bytes } from '../lib/bytes';
  import DomainShell from '../lib/DomainShell.svelte';
  import MailRulesForm from '../lib/MailRules.svelte';
  import MailboxTransfers from '../lib/MailboxTransfers.svelte';
  import ErrorNote from '../lib/ErrorNote.svelte';
  import { labelled } from '../lib/stack';
  import ListBar from '../lib/ListBar.svelte';
  import { t, tFor } from '../i18n';

  const MIN = 8;
  const when = (s: number): string => new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'short' }).format(new Date(s * 1000));
  let { app, me, name }: { app: App; me: Me; name: string } = $props();

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

  // The account whose deletion is being confirmed: the delete button asks first, because it cannot be undone.
  let deleting = $state<string | null>(null);
  const remove = (a: Account): Promise<void> =>
    guarded(async () => {
      await app.client.deleteAccount(a.address);
      notice = t('accounts.deleteScheduled', { address: a.address });
      deleting = null;
      await load();
    });

  // Making many accounts from a CSV (a file or pasted text): one line per account, sent in requests of at most 400 lines; the dry run only reports.
  const CHUNK = 400;
  let csv = $state('');
  let report = $state<ImportResult | null>(null);

  async function readFile(e: Event): Promise<void> {
    const file = (e.currentTarget as HTMLInputElement).files?.[0];
    if (file !== undefined) csv = await file.text();
  }

  const runImport = (dry: boolean): Promise<void> =>
    guarded(async () => {
      const lines = csv.split(/\r?\n/);
      const total: ImportResult = { dry_run: dry, created: 0, exists: 0, invalid: 0, rows: [] };
      for (let i = 0; i < lines.length; i += CHUNK) {
        const part = await app.client.importAccounts(name, lines.slice(i, i + CHUNK).join('\n'), dry);
        total.created += part.created;
        total.exists += part.exists;
        total.invalid += part.invalid;
        // A line number is the number within its request: shift it to the one in the file.
        total.rows.push(...part.rows.filter((r) => r.result === 'invalid').map((r) => ({ ...r, line: r.line + i })));
      }
      report = total;
      if (!dry) await load();
    });

  let editing = $state<string | null>(null);
  let fresh = $state('');
  let temporary = $state(false);
  // The account whose forwarding and automatic reply are open.
  let rulesFor = $state<string | null>(null);
  // The account whose mailbox is being exported or imported (platform administrators).
  let transfersFor = $state<string | null>(null);

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
      await app.client.setPassword(address, fresh, temporary);
      notice = t('accounts.passwordSet', { address });
      editing = null;
      temporary = false;
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
              <th scope="col">{t('accounts.lastDelivery')}</th>
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
                <td>{a.last_delivery_at === null ? t('common.never') : when(a.last_delivery_at)}</td>
                <td class="col-actions">
                  <div class="row-actions">
                    {#if a.status === 'active'}
                      <button type="button" disabled={busy} aria-label={t('accounts.lock', { address: a.address })} onclick={() => change(a, 'lock')}>{t('accounts.lockShort')}</button>
                      <button type="button" disabled={busy} aria-label={t('accounts.disable', { address: a.address })} onclick={() => change(a, 'disable')}>{t('accounts.disableShort')}</button>
                    {:else}
                      <button type="button" disabled={busy} aria-label={t('accounts.enable', { address: a.address })} onclick={() => change(a, 'enable')}>{t('accounts.enableShort')}</button>
                    {/if}
                    {#if a.status === 'disabled'}
                      <button type="button" disabled={busy} aria-label={t('accounts.delete', { address: a.address })} onclick={() => (deleting = deleting === a.address ? null : a.address)}>{t('accounts.deleteShort')}</button>
                    {/if}
                    {#if me.scope === 'platform'}
                      <button type="button" aria-label={t('transfers.open', { address: a.address })} onclick={() => (transfersFor = transfersFor === a.address ? null : a.address)}>{t('transfers.openShort')}</button>
                    {/if}
                    <button type="button" aria-label={t('rules.open', { address: a.address })} onclick={() => (rulesFor = rulesFor === a.address ? null : a.address)}>{t('rules.openShort')}</button>
                    <button type="button" aria-label={t('accounts.setPassword', { address: a.address })} onclick={() => { editing = editing === a.address ? null : a.address; fresh = ''; tooShort = false; }}>{t('accounts.setPasswordShort')}</button>
                  </div>
                  {#if transfersFor === a.address}
                    <MailboxTransfers {app} address={a.address} onclose={() => (transfersFor = null)} />
                  {/if}
                  {#if rulesFor === a.address}
                    <MailRulesForm {app} address={a.address} onclose={() => (rulesFor = null)} />
                  {/if}
                  {#if deleting === a.address}
                    <div class="row-form" role="alert">
                      <p>{t('accounts.deleteAsk', { address: a.address })}</p>
                      <div class="form-actions">
                        <button type="button" class="danger" disabled={busy} onclick={() => remove(a)}>{t('accounts.deleteYes')}</button>
                        <button type="button" class="secondary" onclick={() => (deleting = null)}>{t('accounts.cancel')}</button>
                      </div>
                    </div>
                  {/if}
                  {#if editing === a.address}
                    <form class="row-form" onsubmit={(e) => changePassword(e, a.address)}>
                      <label>
                        {t('accounts.passwordFor', { address: a.address })}
                        <input bind:value={fresh} name="password" type="password" autocomplete="new-password" required />
                      </label>
                      <label class="check"><input type="checkbox" bind:checked={temporary} /> {t('accounts.mustChange')}</label>
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

    <section class="panel" aria-labelledby="import-accounts">
      <h2 id="import-accounts">{t('import.title')}</h2>
      <p class="hint">{t('import.lede')}</p>
      <label>{t('import.file')}<input type="file" accept=".csv,text/csv,text/plain" onchange={readFile} /></label>
      <label>{t('import.text')}<textarea bind:value={csv} name="csv" rows="6" spellcheck="false"></textarea></label>
      <div class="form-actions">
        <button type="button" class="secondary" disabled={busy || csv.trim() === ''} onclick={() => runImport(true)}>{t('import.check')}</button>
        <button type="button" class="primary" disabled={busy || csv.trim() === ''} onclick={() => runImport(false)}>{t('import.run')}</button>
      </div>
      {#if report !== null}
        <p class="alert ok" role="status">{report.dry_run ? t('import.checked', { n: report.created, exists: report.exists, bad: report.invalid }) : t('import.done', { n: report.created, exists: report.exists, bad: report.invalid })}</p>
        {#if report.rows.length > 0}
          <ul class="members">
            {#each report.rows as r (r.line)}<li>{t('import.line', { line: r.line, address: r.address, reason: r.reason ?? '' })}</li>{/each}
          </ul>
        {/if}
      {/if}
    </section>

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
