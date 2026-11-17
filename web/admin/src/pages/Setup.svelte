<script lang="ts">
  import { onMount } from 'svelte';
  import type { App } from '../lib/app.svelte';
  import ErrorNote from '../lib/ErrorNote.svelte';
  import { t } from '../i18n';

  const MIN = 12;
  let { app }: { app: App } = $props();
  let token = $state('');
  let address = $state('');
  let password = $state('');
  let confirm = $state('');
  let busy = $state(false);
  let error = $state<unknown>(null);
  let problem = $state<'mismatch' | 'short' | null>(null);
  let first: HTMLInputElement | undefined = $state();

  onMount(() => first?.focus());

  async function submit(e: SubmitEvent): Promise<void> {
    e.preventDefault();
    error = null;
    problem = null;
    if ([...password].length < MIN) {
      problem = 'short';
      return;
    }
    if (password !== confirm) {
      problem = 'mismatch';
      return;
    }
    busy = true;
    try {
      app.signedIn(await app.client.setup(token.trim(), address, password));
    } catch (err) {
      error = err;
    } finally {
      busy = false;
    }
  }
</script>

<main class="auth" id="content">
  <div class="auth-card">
    <div class="auth-brand">
      <span class="brand-mark" aria-hidden="true">{t('app.mark')}</span>
      <div>
        <div class="brand-name">{t('app.title')}</div>
        <div class="brand-sub">{t('app.productSub')}</div>
      </div>
    </div>
    <div class="panel">
      <h1>{t('setup.title')}</h1>
      <p class="lede">{t('setup.intro')}</p>
    <form onsubmit={submit}>
      <label>
        {t('setup.token')}
        <input bind:this={first} bind:value={token} name="token" type="password" autocomplete="off" autocapitalize="none" spellcheck="false" required />
      </label>
      <label>
        {t('setup.address')}
        <input bind:value={address} name="address" type="text" inputmode="email" autocomplete="username" autocapitalize="none" spellcheck="false" required />
      </label>
      <label>
        {t('setup.password', { min: MIN })}
        <input bind:value={password} name="password" type="password" autocomplete="new-password" required />
      </label>
      <label>
        {t('setup.confirm')}
        <input bind:value={confirm} name="confirm" type="password" autocomplete="new-password" required />
      </label>
      {#if problem === 'short'}<p class="note" role="alert">{t('setup.tooShort', { min: MIN })}</p>{/if}
      {#if problem === 'mismatch'}<p class="note" role="alert">{t('setup.mismatch')}</p>{/if}
      {#if error !== null}<ErrorNote {error} />{/if}
      <button type="submit" disabled={busy}>{busy ? t('setup.submitting') : t('setup.submit')}</button>
    </form>
    </div>
  </div>
</main>
