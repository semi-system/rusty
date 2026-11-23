<script lang="ts">
  import { onMount } from 'svelte';
  import type { App } from '../lib/app.svelte';
  import ErrorNote from '../lib/ErrorNote.svelte';
  import PageHead from '../lib/PageHead.svelte';
  import { megabytes } from '../lib/bytes';
  import { t } from '../i18n';

  let { app }: { app: App } = $props();

  let loaded = $state(false);
  let error = $state<unknown>(null);
  let notice = $state<string | null>(null);
  let invalid = $state(false);
  let busy = $state(false);

  // The form holds text: an empty field means "none".
  let quotaMb = $state('');
  let maxMessageMb = $state('');
  let perHour = $state('');
  // What happens to a message with a virus: refused (the default) or filed in the Spam folder.
  let virusAction = $state('reject');
  let blockedText = $state('');
  let adminSearch = $state(false);

  const mb = (n: number | null): string => (n === null ? '' : String(n / 1_048_576));

  onMount(async () => {
    try {
      const s = await app.client.platformSettings();
      quotaMb = mb(s.quota_bytes);
      maxMessageMb = mb(s.max_message_bytes);
      perHour = s.messages_per_hour === null ? '' : String(s.messages_per_hour);
      virusAction = s.virus_action ?? 'reject';
      blockedText = s.blocked_extensions.join(' ');
      adminSearch = s.admin_search;
      loaded = true;
    } catch (e) {
      error = e;
    }
  });

  async function save(e: SubmitEvent): Promise<void> {
    e.preventDefault();
    const quota = megabytes(quotaMb);
    const message = megabytes(maxMessageMb);
    const hours = perHour.trim() === '' ? null : Number(perHour);
    invalid = Number.isNaN(quota) || Number.isNaN(message) || (hours !== null && (!Number.isInteger(hours) || hours < 1));
    if (invalid) return;
    busy = true;
    error = null;
    notice = null;
    try {
      await app.client.setPlatformSettings({ quota_bytes: quota, max_message_bytes: message, messages_per_hour: hours, virus_action: virusAction === 'reject' ? null : virusAction, blocked_extensions: blockedText.split(/[\s,]+/).filter((x) => x !== ''), admin_search: adminSearch });
      notice = t('platform.saved');
    } catch (err) {
      error = err;
    } finally {
      busy = false;
    }
  }
</script>

<main id="content" tabindex="-1">
  <PageHead title={t('platform.title')} lede={t('platform.lede')} />
  {#if error !== null}<ErrorNote {error} />{/if}
  {#if invalid}<p class="alert danger" role="alert">{t('platform.invalid')}</p>{/if}
  {#if notice !== null}<p class="alert ok" role="status">{notice}</p>{/if}
  {#if !loaded}
    <p class="muted" role="status">{t('app.loading')}</p>
  {:else}
    <form onsubmit={save}>
      <div class="field-row">
        <label>{t('platform.quotaMb')}<input bind:value={quotaMb} name="quota" type="text" inputmode="decimal" /></label>
        <label>{t('platform.maxMessageMb')}<input bind:value={maxMessageMb} name="message" type="text" inputmode="decimal" /></label>
        <label>{t('platform.perHour')}<input bind:value={perHour} name="perhour" type="text" inputmode="numeric" /></label>
      </div>
      <div class="field-row">
        <label>
          {t('platform.virus')}
          <select bind:value={virusAction} name="virus">
            <option value="reject">{t('platform.virusReject')}</option>
            <option value="quarantine">{t('platform.virusQuarantine')}</option>
          </select>
        </label>
      </div>
      <div class="field-row">
        <label>{t('platform.blocked')}<input bind:value={blockedText} name="blocked" type="text" autocapitalize="none" spellcheck="false" /></label>
      </div>
      <p class="hint">{t('platform.blockedHelp')}</p>
      <fieldset class="checks">
        <legend>{t('platform.searchLegend')}</legend>
        <label><input type="checkbox" bind:checked={adminSearch} /> {t('platform.search')}</label>
        <p class="hint">{t('platform.searchHelp')}</p>
      </fieldset>
      <div class="form-actions"><button type="submit" class="primary" disabled={busy}>{busy ? t('common.saving') : t('common.save')}</button></div>
    </form>
  {/if}
</main>
