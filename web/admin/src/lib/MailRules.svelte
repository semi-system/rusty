<script lang="ts">
  import { onMount } from 'svelte';
  import type { App } from '../lib/app.svelte';
  import type { MailRules } from '../api/types';
  import ErrorNote from './ErrorNote.svelte';
  import { t } from '../i18n';

  let { app, address, onclose }: { app: App; address: string; onclose: () => void } = $props();

  let loaded = $state(false);
  let error = $state<unknown>(null);
  let notice = $state<string | null>(null);
  let busy = $state(false);

  // One forwarding address a line; an empty box means no forwarding.
  let forwardText = $state('');
  let vacationOn = $state(false);
  let subject = $state('');
  let body = $state('');
  // The start and the end of the reply are kept as they are stored (set through the API); this form does not change them.
  let from = $state<number | null>(null);
  let until = $state<number | null>(null);

  onMount(async () => {
    try {
      const r = await app.client.mailRules(address);
      forwardText = (r.forward?.to ?? []).join('\n');
      vacationOn = r.vacation?.enabled ?? false;
      subject = r.vacation?.subject ?? '';
      body = r.vacation?.body ?? '';
      from = r.vacation?.from ?? null;
      until = r.vacation?.until ?? null;
      loaded = true;
    } catch (e) {
      error = e;
    }
  });

  async function save(e: SubmitEvent): Promise<void> {
    e.preventDefault();
    const to = forwardText.split('\n').map((l) => l.trim()).filter((l) => l !== '');
    const rules: Partial<MailRules> = {};
    if (to.length > 0) rules.forward = { to, keep: true };
    if (vacationOn || body.trim() !== '') rules.vacation = { enabled: vacationOn, subject: subject.trim(), body, from, until };
    busy = true;
    error = null;
    notice = null;
    try {
      await app.client.setMailRules(address, rules);
      notice = t('rules.saved');
    } catch (err) {
      error = err;
    } finally {
      busy = false;
    }
  }
</script>

<form class="row-form" onsubmit={save}>
  <h3>{t('rules.title', { address })}</h3>
  {#if error !== null}<ErrorNote {error} />{/if}
  {#if notice !== null}<p class="alert ok" role="status">{notice}</p>{/if}
  {#if !loaded && error === null}
    <p class="muted" role="status">{t('app.loading')}</p>
  {:else if loaded}
    <label>{t('rules.forward')}<textarea bind:value={forwardText} name="forward" rows="3" autocapitalize="none" spellcheck="false"></textarea></label>
    <p class="hint">{t('rules.forwardHelp')}</p>
    <label class="check"><input type="checkbox" bind:checked={vacationOn} name="vacation" /> {t('rules.vacation')}</label>
    <label>{t('rules.subject')}<input bind:value={subject} name="subject" type="text" maxlength="200" /></label>
    <label>{t('rules.body')}<textarea bind:value={body} name="body" rows="4" maxlength="5000"></textarea></label>
    <p class="hint">{t('rules.vacationHelp')}</p>
    <div class="form-actions">
      <button type="submit" class="primary" disabled={busy}>{busy ? t('common.saving') : t('common.save')}</button>
      <button type="button" class="secondary" onclick={onclose}>{t('rules.close')}</button>
    </div>
  {/if}
</form>
