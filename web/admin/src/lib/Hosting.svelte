<script lang="ts">
  import { onMount } from 'svelte';
  import type { App } from './app.svelte';
  import type { DnsHosting } from '../api/types';
  import Copy from './Copy.svelte';
  import ErrorNote from './ErrorNote.svelte';
  import { labelled } from './stack';
  import { t } from '../i18n';

  let { app, name }: { app: App; name: string } = $props();

  let data = $state<DnsHosting | null>(null);
  let error = $state<unknown>(null);
  let busy = $state(false);
  let rname = $state('');
  let rtype = $state('A');
  let rvalue = $state('');
  let rttl = $state('3600');
  let invalid = $state(false);

  const platform = $derived(app.view.kind === 'in' && app.view.me.scope === 'platform');

  async function load(): Promise<void> {
    try {
      data = await app.client.dnsHosting(name);
    } catch (e) {
      error = e;
    }
  }
  onMount(() => void load());

  async function run(work: () => Promise<DnsHosting>): Promise<void> {
    busy = true;
    error = null;
    try {
      data = await work();
    } catch (e) {
      error = e;
    } finally {
      busy = false;
    }
  }

  const setMode = (mode: string): Promise<void> => run(() => app.client.setDnsHosting(name, mode));

  async function add(e: SubmitEvent): Promise<void> {
    e.preventDefault();
    invalid = false;
    const ttl = Number(rttl);
    if (rname.trim() === '' || rvalue.trim() === '' || !Number.isInteger(ttl)) {
      invalid = true;
      return;
    }
    await run(() => app.client.addDnsExtra(name, { name: rname, type: rtype, value: rvalue, ttl }));
    if (error === null) {
      rname = '';
      rvalue = '';
    }
  }
</script>

<section aria-labelledby="hosting-title">
  <h3 id="hosting-title">{t('hosting.title')}</h3>
  <p class="muted">{t('hosting.intro')}</p>
  {#if error !== null}<ErrorNote {error} />{/if}
  {#if data !== null}
    <p>
      {data.mode === 'hosted' ? t('hosting.modeHosted') : t('hosting.modeExternal')}
      {#if platform}
        <button type="button" class="secondary" disabled={busy} onclick={() => setMode(data?.mode === 'hosted' ? 'external' : 'hosted')}>
          {data.mode === 'hosted' ? t('hosting.turnOff') : t('hosting.turnOn')}
        </button>
      {/if}
    </p>
    {#if data.mode === 'hosted'}
      <p>{t('hosting.delegate')}</p>
      <ul class="plain" aria-label={t('hosting.nameServers')}>
        {#each data.name_servers as ns (ns)}<li><Copy value={ns} label={t('hosting.nameServers')} /></li>{/each}
      </ul>
      <form onsubmit={add} aria-label={t('hosting.add')}>
        <div class="field-row">
          <label>{t('dns.name')}<input bind:value={rname} name="name" type="text" autocapitalize="none" spellcheck="false" /></label>
          <label>{t('dns.type')}
            <select name="type" bind:value={rtype}>
              {#each ['A', 'AAAA', 'CNAME', 'TXT', 'MX', 'SRV'] as k (k)}<option value={k}>{k}</option>{/each}
            </select>
          </label>
          <label>{t('dns.value')}<input bind:value={rvalue} name="value" type="text" autocapitalize="none" spellcheck="false" /></label>
          <label>{t('hosting.ttl')}<input bind:value={rttl} name="ttl" type="text" inputmode="numeric" /></label>
          <button type="submit" class="primary" disabled={busy}>{t('hosting.add')}</button>
        </div>
        {#if invalid}<p class="alert danger" role="alert">{t('hosting.invalid')}</p>{/if}
      </form>
      {#if data.extra.length === 0}
        <p class="muted">{t('hosting.none')}</p>
      {:else}
        <div class="table-wrap">
          <table use:labelled aria-label={t('hosting.list')}>
            <thead>
              <tr>
                <th scope="col">{t('dns.name')}</th>
                <th scope="col">{t('dns.type')}</th>
                <th scope="col">{t('dns.value')}</th>
                <th scope="col">{t('hosting.ttl')}</th>
                <th scope="col" class="col-actions">{t('domains.col.action')}</th>
              </tr>
            </thead>
            <tbody>
              {#each data.extra as r (r.id)}
                <tr>
                  <th scope="row"><span class="mono">{r.name}</span></th>
                  <td><span class="mono">{r.type}</span></td>
                  <td><span class="mono">{r.value}</span></td>
                  <td>{r.ttl}</td>
                  <td class="col-actions"><button type="button" class="link" disabled={busy} onclick={() => run(() => app.client.removeDnsExtra(name, r.id))}>{t('hosting.remove')}</button></td>
                </tr>
              {/each}
            </tbody>
          </table>
        </div>
      {/if}
    {/if}
  {/if}
</section>
