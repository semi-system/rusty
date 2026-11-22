<script lang="ts">
  import { onMount } from 'svelte';
  import type { App } from '../lib/app.svelte';
  import type { Me, Member, MailList, Routing } from '../api/types';
  import DomainShell from '../lib/DomainShell.svelte';
  import ErrorNote from '../lib/ErrorNote.svelte';
  import { labelled } from '../lib/stack';
  import { t } from '../i18n';

  let { app, me, name }: { app: App; me: Me; name: string } = $props();

  let routing = $state<Routing | null>(null);
  let error = $state<unknown>(null);
  let notice = $state<string | null>(null);
  let busy = $state(false);

  // The list whose members are shown, and what is typed in the forms.
  let open = $state<string | null>(null);
  let members = $state<Member[]>([]);
  let listName = $state('');
  let member = $state('');
  let catchAll = $state('');
  let aliasName = $state('');
  let deleting = $state<string | null>(null);

  async function load(): Promise<void> {
    try {
      routing = await app.client.routing(name);
      catchAll = routing.catch_all ?? '';
    } catch (e) {
      error = e;
    }
  }

  onMount(() => void load());

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

  async function show(list: MailList): Promise<void> {
    if (open === list.local) {
      open = null;
      return;
    }
    await guarded(async () => {
      members = await app.client.listMembers(name, list.local);
      open = list.local;
    });
  }

  const createList = (e: SubmitEvent): Promise<void> => {
    e.preventDefault();
    return guarded(async () => {
      const made = await app.client.createList(name, listName.trim());
      notice = t('routing.listCreated', { address: made.address });
      listName = '';
      await load();
    });
  };

  const removeList = (list: MailList): Promise<void> =>
    guarded(async () => {
      await app.client.deleteList(name, list.local);
      notice = t('routing.listDeleted', { address: list.address });
      deleting = null;
      if (open === list.local) open = null;
      await load();
    });

  const addMember = (e: SubmitEvent, list: MailList): Promise<void> => {
    e.preventDefault();
    return guarded(async () => {
      members = await app.client.addListMember(name, list.local, member.trim());
      member = '';
      await load();
    });
  };

  const removeMember = (list: MailList, m: Member): Promise<void> =>
    guarded(async () => {
      await app.client.removeListMember(name, list.local, m.seq);
      members = members.filter((x) => x.seq !== m.seq);
      await load();
    });

  const saveCatchAll = (e: SubmitEvent): Promise<void> => {
    e.preventDefault();
    return guarded(async () => {
      const local = catchAll.trim() === '' ? null : catchAll.trim();
      await app.client.setCatchAll(name, local);
      notice = local === null ? t('routing.catchAllCleared') : t('routing.catchAllSet', { address: `${local}@${name}` });
      await load();
    });
  };

  const addAlias = (e: SubmitEvent): Promise<void> => {
    e.preventDefault();
    return guarded(async () => {
      await app.client.addDomainAlias(name, aliasName.trim());
      notice = t('routing.aliasAdded', { name: aliasName.trim().toLowerCase() });
      aliasName = '';
      await load();
    });
  };

  const removeAlias = (alias: string): Promise<void> =>
    guarded(async () => {
      await app.client.removeDomainAlias(name, alias);
      await load();
    });
</script>

<main id="content" tabindex="-1">
  <DomainShell {name}>
    <div class="section-head">
      <h2>{t('routing.title', { domain: name })}</h2>
      <p>{t('routing.lede')}</p>
    </div>
    {#if error !== null}<ErrorNote {error} />{/if}
    {#if notice !== null}<p class="alert ok" role="status">{notice}</p>{/if}

    {#if routing === null}
      <p class="muted" role="status">{t('app.loading')}</p>
    {:else}
      <section class="panel" aria-labelledby="lists">
        <h2 id="lists">{t('routing.lists')}</h2>
        <p class="hint">{t('routing.listsLede')}</p>
        {#if routing.lists.length === 0}
          <div class="empty"><p>{t('routing.listsEmpty')}</p></div>
        {:else}
          <div class="table-wrap">
            <table use:labelled aria-label={t('routing.lists')}>
              <thead>
                <tr><th scope="col">{t('routing.address')}</th><th scope="col">{t('routing.members')}</th><th scope="col" class="col-actions"><span class="sr">{t('domains.col.action')}</span></th></tr>
              </thead>
              <tbody>
                {#each routing.lists as l (l.local)}
                  <tr>
                    <th scope="row">{l.address}</th>
                    <td>{l.members}</td>
                    <td class="col-actions">
                      <div class="row-actions">
                        <button type="button" disabled={busy} aria-label={t('routing.showMembers', { address: l.address })} onclick={() => show(l)}>{open === l.local ? t('routing.hide') : t('routing.membersShort')}</button>
                        <button type="button" disabled={busy} aria-label={t('routing.deleteList', { address: l.address })} onclick={() => (deleting = deleting === l.local ? null : l.local)}>{t('accounts.deleteShort')}</button>
                      </div>
                      {#if deleting === l.local}
                        <div class="row-form" role="alert">
                          <p>{t('routing.deleteAsk', { address: l.address })}</p>
                          <div class="form-actions">
                            <button type="button" class="danger" disabled={busy} onclick={() => removeList(l)}>{t('routing.deleteYes')}</button>
                            <button type="button" class="secondary" onclick={() => (deleting = null)}>{t('accounts.cancel')}</button>
                          </div>
                        </div>
                      {/if}
                      {#if open === l.local}
                        <ul class="members">
                          {#each members as m (m.seq)}
                            <li>{m.address} <button type="button" disabled={busy} aria-label={t('routing.removeMember', { address: m.address })} onclick={() => removeMember(l, m)}>{t('routing.remove')}</button></li>
                          {:else}
                            <li class="muted">{t('routing.noMembers')}</li>
                          {/each}
                        </ul>
                        <form class="row-form" onsubmit={(e) => addMember(e, l)}>
                          <label>{t('routing.newMember')}<input bind:value={member} name="member" type="email" autocapitalize="none" spellcheck="false" required /></label>
                          <div class="form-actions"><button type="submit" disabled={busy}>{t('routing.add')}</button></div>
                        </form>
                      {/if}
                    </td>
                  </tr>
                {/each}
              </tbody>
            </table>
          </div>
        {/if}
        <form onsubmit={createList}>
          <div class="field-row">
            <label>{t('routing.listName')}<input bind:value={listName} name="list" type="text" autocapitalize="none" spellcheck="false" required /></label>
            <button type="submit" disabled={busy}>{busy ? t('common.creating') : t('common.create')}</button>
          </div>
        </form>
      </section>

      <section class="panel" aria-labelledby="catch-all">
        <h2 id="catch-all">{t('routing.catchAll')}</h2>
        <p class="hint">{t('routing.catchAllLede')}</p>
        <form onsubmit={saveCatchAll}>
          <div class="field-row">
            <label>{t('routing.catchAllLocal')}<input bind:value={catchAll} name="catchall" type="text" autocapitalize="none" spellcheck="false" /></label>
            <button type="submit" class="primary" disabled={busy}>{busy ? t('common.saving') : t('common.save')}</button>
          </div>
        </form>
      </section>

      <section class="panel" aria-labelledby="domain-aliases">
        <h2 id="domain-aliases">{t('routing.domainAliases')}</h2>
        <p class="hint">{t('routing.domainAliasesLede', { domain: name })}</p>
        {#if routing.domain_aliases.length === 0}
          <p class="muted">{t('routing.noDomainAliases')}</p>
        {:else}
          <ul class="members">
            {#each routing.domain_aliases as a (a)}
              <li>{a}{#if me.scope === 'platform'} <button type="button" disabled={busy} aria-label={t('routing.removeAlias', { name: a })} onclick={() => removeAlias(a)}>{t('routing.remove')}</button>{/if}</li>
            {/each}
          </ul>
        {/if}
        {#if me.scope === 'platform'}
          <form onsubmit={addAlias}>
            <div class="field-row">
              <label>{t('routing.aliasName')}<input bind:value={aliasName} name="alias" type="text" autocapitalize="none" spellcheck="false" required /></label>
              <button type="submit" disabled={busy}>{t('routing.add')}</button>
            </div>
          </form>
        {/if}
      </section>
    {/if}
  </DomainShell>
</main>
