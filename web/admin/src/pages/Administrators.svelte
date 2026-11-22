<script lang="ts">
  import { onMount } from 'svelte';
  import type { App } from '../lib/app.svelte';
  import { ApiError } from '../api/client';
  import type { AdminEntry, Domain, Me } from '../api/types';
  import Badge from '../lib/Badge.svelte';
  import ErrorNote from '../lib/ErrorNote.svelte';
  import ListBar from '../lib/ListBar.svelte';
  import PageHead from '../lib/PageHead.svelte';
  import { labelled } from '../lib/stack';
  import { t } from '../i18n';

  let { app, me }: { app: App; me: Me } = $props();

  const platform = $derived(me.scope === 'platform');

  let items = $state<AdminEntry[]>([]);
  let next = $state<number | null>(null);
  let domains = $state<Domain[]>([]);
  let loaded = $state(false);
  let q = $state('');
  let status = $state('');
  let limit = $state(100);
  let total = $state<number | null>(null);
  const narrowed = $derived(q.trim() !== '' || status !== '');
  let error = $state<unknown>(null);
  let notice = $state<string | null>(null);
  let failure = $state<string | null>(null);
  let busy = $state(false);
  let address = $state('');
  let domain = $state('');
  let helpdesk = $state(false);

  async function load(after?: number, quiet = false): Promise<void> {
    try {
      const page = await app.client.admins(after, { q, limit });
      total = page.total;
      items = after === undefined ? page.items : [...items, ...page.items];
      next = page.next;
    } catch (e) {
      if (!quiet) error = e;
    }
    loaded = true;
  }

  // The domains an administrator can be limited to; the list is a convenience for the choice.
  async function loadDomains(): Promise<void> {
    if (!platform) return;
    try {
      let after: number | undefined;
      for (;;) {
        const page = await app.client.domains(after);
        domains = [...domains, ...page.items];
        if (page.next === null) break;
        after = page.next;
      }
    } catch {
      domains = [];
    }
  }

  onMount(() => {
    void load();
    void loadDomains();
    // A hint refreshes the first page in the background: nothing is shown when that fails (the next hint tries again).
    return app.live.watch(['admins'], () => void load(undefined, true));
  });

  // What the server refused, in this page's words (the generic text would say only "already exists").
  function explain(e: unknown, own: string): string | null {
    if (!(e instanceof ApiError)) return null;
    if (own === 'revoke' && e.status === 409) return t('admins.last');
    if (own === 'revoke' && e.status === 400) return t('admins.self');
    if (own === 'grant' && e.status === 400) return t('admins.noPassword');
    if (own === 'grant' && e.status === 404) return t('admins.unknownAccount');
    return null;
  }

  async function guarded(own: string, work: () => Promise<void>): Promise<void> {
    busy = true;
    error = null;
    failure = null;
    notice = null;
    try {
      await work();
    } catch (e) {
      failure = explain(e, own);
      error = failure === null ? e : null;
    } finally {
      busy = false;
    }
  }
  const grant = (e: SubmitEvent): Promise<void> => {
    e.preventDefault();
    return guarded('grant', async () => {
      const g = await app.client.grantAdmin(address.trim(), domain === '' ? null : domain, helpdesk && domain !== '');
      notice = t('admins.added', { address: g.address });
      address = '';
      await load();
    });
  };

  const revoke = (a: AdminEntry): Promise<void> => {
    return guarded('revoke', async () => {
      await app.client.revokeAdmin(a.address);
      notice = t('admins.removed', { address: a.address });
      items = items.filter((x) => x.id !== a.id);
    });
  };

  const when = (s: number): string => new Intl.DateTimeFormat(undefined, { dateStyle: 'medium' }).format(new Date(s * 1000));
</script>

<main id="content" tabindex="-1">
  <PageHead title={t('admins.title')} lede={t('admins.lede')} />

  {#if failure !== null}<p class="alert danger" role="alert">{failure}</p>{:else if error !== null}<ErrorNote {error} />{/if}
  {#if notice !== null}<p class="alert ok" role="status">{notice}</p>{/if}

  {#if loaded}
    <ListBar bind:q bind:status bind:limit {total} shown={items.length} label={t('list.searchAdmins')} onchange={() => void load()} />
  {/if}
  {#if !loaded}
    <p class="muted" role="status">{t('app.loading')}</p>
  {:else if items.length === 0 && error === null}
    <div class="empty"><p>{narrowed ? t('list.noMatch') : t('admins.empty')}</p></div>
  {:else}
    <div class="table-wrap">
      <table use:labelled aria-label={t('admins.list')}>
        <thead>
          <tr>
            <th scope="col">{t('admins.account')}</th>
            <th scope="col">{t('admins.scope')}</th>
            <th scope="col">{t('admins.since')}</th>
            {#if platform}<th scope="col" class="col-actions"><span class="sr">{t('admins.title')}</span></th>{/if}
          </tr>
        </thead>
        <tbody>
          {#each items as a (a.id)}
            <tr>
              <th scope="row">{a.address}</th>
              <td><Badge tone={a.scope === 'platform' ? 'ok' : 'neutral'}>{a.domain === null ? t('admins.scopePlatform') : t('admins.scopeDomain', { domain: a.domain })}</Badge>{#if a.role === 'helpdesk'} <Badge tone="warn">{t('admins.roleHelpdesk')}</Badge>{/if}</td>
              <td>{when(a.created_at)}</td>
              {#if platform}
                <td class="col-actions">
                  {#if a.address !== me.address}
                    <button type="button" class="danger" disabled={busy} aria-label={t('admins.remove', { address: a.address })} onclick={() => revoke(a)}>{t('common.remove')}</button>
                  {/if}
                </td>
              {/if}
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
    {#if next !== null}<p class="form-actions"><button type="button" class="secondary" onclick={() => load(next ?? undefined)}>{t('common.loadMore')}</button></p>{/if}
  {/if}

  {#if platform}
    <section class="panel" aria-labelledby="new-admin">
      <h2 id="new-admin">{t('admins.new')}</h2>
      <p class="hint">{t('admins.newLede')}</p>
      <form onsubmit={grant}>
        <div class="field-row">
          <label>
            {t('admins.address')}
            <input bind:value={address} name="address" type="text" inputmode="email" autocapitalize="none" spellcheck="false" required />
          </label>
          <label>
            {t('admins.domain')}
            <select bind:value={domain} name="domain">
              <option value="">{t('admins.allDomains')}</option>
              {#each domains as d (d.id)}<option value={d.name}>{d.name}</option>{/each}
            </select>
          </label>
          <label class="check"><input type="checkbox" bind:checked={helpdesk} disabled={domain === ''} /> {t('admins.helpdesk')}</label>
          <button type="submit" disabled={busy}>{busy ? t('admins.adding') : t('admins.add')}</button>
        </div>
      </form>
    </section>
  {/if}
</main>
