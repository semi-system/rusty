<script lang="ts">
  import { onMount } from 'svelte';
  import type { App } from '../lib/app.svelte';
  import ErrorNote from '../lib/ErrorNote.svelte';
  import { t } from '../i18n';

  let { app }: { app: App } = $props();
  let address = $state('');
  let password = $state('');
  let busy = $state(false);
  let error = $state<unknown>(null);
  let first: HTMLInputElement | undefined = $state();

  onMount(() => first?.focus());

  async function submit(e: SubmitEvent): Promise<void> {
    e.preventDefault();
    busy = true;
    error = null;
    try {
      app.signedIn(await app.client.signIn(address, password));
    } catch (err) {
      error = err;
      password = '';
    } finally {
      busy = false;
    }
  }
</script>

<main id="content">
  <h1>{t('login.title')}</h1>
  <form onsubmit={submit}>
    <label>
      {t('login.address')}
      <input bind:this={first} bind:value={address} name="address" type="text" inputmode="email" autocomplete="username" autocapitalize="none" spellcheck="false" required />
    </label>
    <label>
      {t('login.password')}
      <input bind:value={password} name="password" type="password" autocomplete="current-password" required />
    </label>
    {#if error !== null}<ErrorNote {error} />{/if}
    <button type="submit" disabled={busy}>{busy ? t('login.submitting') : t('login.submit')}</button>
  </form>
</main>
