<script lang="ts">
  import { onMount } from 'svelte';
  import type { App } from '../lib/app.svelte';
  import type { Relay } from '../api/types';
  import type { RelayInput } from '../api/client';
  import DomainShell from '../lib/DomainShell.svelte';
  import ErrorNote from '../lib/ErrorNote.svelte';
  import { t, tFor } from '../i18n';

  let { app, name }: { app: App; name: string } = $props();

  let relay = $state<Relay | null>(null);
  let loaded = $state(false);
  let error = $state<unknown>(null);
  let notice = $state<string | null>(null);
  let busy = $state(false);

  let host = $state('');
  let port = $state(587);
  let tls = $state<'starttls' | 'implicit' | 'none'>('starttls');
  let username = $state('');
  let password = $state('');
  let spf = $state('');

  async function load(): Promise<void> {
    try {
      relay = await app.client.relay(name);
      if (relay !== null) {
        host = relay.host;
        port = relay.port;
        tls = relay.tls === 'implicit' || relay.tls === 'none' ? relay.tls : 'starttls';
        username = relay.username ?? '';
        spf = relay.spf_include ?? '';
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
      const input: RelayInput = { host, port, tls };
      if (username !== '') {
        input.username = username;
        input.password = password;
      }
      if (spf !== '') input.spf_include = spf;
      relay = await app.client.putRelay(name, input);
      // The password is never kept on the page once it has been sent.
      password = '';
      notice = t('relay.saved');
    });
  };

  const remove = (): Promise<void> =>
    guarded(async () => {
      await app.client.deleteRelay(name);
      relay = null;
      host = '';
      username = '';
      password = '';
      spf = '';
      notice = t('relay.removed');
    });

  const check = (): Promise<void> =>
    guarded(async () => {
      await app.client.checkRelay(name);
      notice = t('relay.checkQueued');
    });

  const when = (s: number): string => new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'short' }).format(new Date(s * 1000));
</script>

<main id="content" tabindex="-1">
  <DomainShell {name}>
    <h2>{t('relay.title', { domain: name })}</h2>
    <p class="muted">{t('relay.intro')}</p>
    {#if !loaded}
      <p class="muted" role="status">{t('app.loading')}</p>
    {:else if relay === null}
      <p class="muted">{t('relay.none')}</p>
    {:else}
      <p>{relay.password_set ? t('relay.passwordSet') : t('relay.passwordNotSet')}</p>
      <p>
        {t('relay.lastCheck')}:
        {#if relay.check_status === null || relay.checked_at === null}
          {t('relay.neverChecked')}
        {:else}
          <span>{tFor('relay.check.', relay.check_status, 'relay.check.unreachable')}</span> <span class="muted">({when(relay.checked_at)})</span>
        {/if}
      </p>
    {/if}
    {#if notice !== null}<p role="status">{notice}</p>{/if}
    {#if error !== null}<ErrorNote {error} />{/if}

    <form onsubmit={save}>
      <label>
        {t('relay.host')}
        <input bind:value={host} name="host" type="text" autocapitalize="none" spellcheck="false" required />
      </label>
      <label>
        {t('relay.port')}
        <input bind:value={port} name="port" type="number" min="1" max="65535" required />
      </label>
      <label>
        {t('relay.tls')}
        <select bind:value={tls} name="tls">
          <option value="starttls">{t('relay.tls.starttls')}</option>
          <option value="implicit">{t('relay.tls.implicit')}</option>
          <option value="none">{t('relay.tls.none')}</option>
        </select>
      </label>
      <label>
        {t('relay.username')}
        <input bind:value={username} name="username" type="text" autocomplete="off" autocapitalize="none" spellcheck="false" />
      </label>
      <label>
        {t('relay.password')}
        <input bind:value={password} name="password" type="password" autocomplete="new-password" />
      </label>
      <label>
        {t('relay.spf')}
        <input bind:value={spf} name="spf" type="text" autocapitalize="none" spellcheck="false" />
      </label>
      <button type="submit" disabled={busy}>{busy ? t('common.saving') : t('relay.save')}</button>
    </form>
    {#if relay !== null}
      <p>
        <button type="button" onclick={check} disabled={busy}>{t('relay.check')}</button>
        <button type="button" onclick={remove} disabled={busy}>{t('relay.remove')}</button>
        <button type="button" onclick={load}>{t('dns.reload')}</button>
      </p>
    {/if}
  </DomainShell>
</main>
