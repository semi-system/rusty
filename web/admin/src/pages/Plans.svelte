<script lang="ts">
  import PageHead from '../lib/PageHead.svelte';
  import { onMount } from 'svelte';
  import type { App } from '../lib/app.svelte';
  import type { Plan } from '../api/types';
  import { Load } from '../lib/load.svelte';
  import ErrorNote from '../lib/ErrorNote.svelte';
  import { labelled } from '../lib/stack';
  import { bytes, megabytes } from '../lib/bytes';
  import { t } from '../i18n';

  let { app }: { app: App } = $props();

  const load = new Load<Plan[]>(() => app.client.plans());
  let busy = $state(false);
  let error = $state<unknown>(null);
  let notice = $state<string | null>(null);
  let invalid = $state(false);

  // The form makes a plan or changes the one of the same name; an empty limit means "none of its own".
  let name = $state('');
  let quota = $state('');
  let maxMessage = $state('');
  let perHour = $state('');
  let junkScore = $state('');
  let rejectScore = $state('');
  let spamDays = $state('');
  let allDays = $state('');
  // The protocols the plan allows; all three checked means the plan does not limit them.
  let protoImap = $state(true);
  let protoPop3 = $state(true);
  let protoSubmission = $state(true);

  onMount(() => void load.run());

  const edit = (p: Plan): void => {
    name = p.name;
    quota = p.quota_bytes === null ? '' : String(p.quota_bytes / 1_048_576);
    maxMessage = p.max_message_bytes === null ? '' : String(p.max_message_bytes / 1_048_576);
    perHour = p.messages_per_hour === null ? '' : String(p.messages_per_hour);
    junkScore = p.spam_junk_score === null ? '' : String(p.spam_junk_score);
    rejectScore = p.spam_reject_score === null ? '' : String(p.spam_reject_score);
    spamDays = p.spam_retention_days === null ? '' : String(p.spam_retention_days);
    allDays = p.retention_days === null ? '' : String(p.retention_days);
    protoImap = p.protocols === null || p.protocols.includes('imap');
    protoPop3 = p.protocols === null || p.protocols.includes('pop3');
    protoSubmission = p.protocols === null || p.protocols.includes('submission');
    notice = null;
    invalid = false;
  };

  async function save(e: SubmitEvent): Promise<void> {
    e.preventDefault();
    const q = megabytes(quota);
    const m = megabytes(maxMessage);
    const hours = perHour.trim() === '' ? null : Number(perHour);
    const junk = junkScore.trim() === '' ? null : Number(junkScore);
    const reject = rejectScore.trim() === '' ? null : Number(rejectScore);
    const spamKeep = spamDays.trim() === '' ? null : Number(spamDays);
    const allKeep = allDays.trim() === '' ? null : Number(allDays);
    const badDays = (x: number | null): boolean => x !== null && (!Number.isInteger(x) || x < 1 || x > 36500);
    const badScore = (x: number | null): boolean => x !== null && (!Number.isFinite(x) || x < 0 || x > 100);
    invalid = Number.isNaN(q) || Number.isNaN(m) || (hours !== null && (!Number.isInteger(hours) || hours < 1)) || badScore(junk) || badScore(reject) || (junk !== null && reject !== null && junk >= reject) || badDays(spamKeep) || badDays(allKeep);
    if (invalid) return;
    const allowed = [protoImap ? 'imap' : '', protoPop3 ? 'pop3' : '', protoSubmission ? 'submission' : ''].filter((x) => x !== '');
    const protocols = allowed.length === 3 ? null : allowed;
    busy = true;
    error = null;
    notice = null;
    try {
      const saved = await app.client.putPlan(name.trim(), q, m, hours, protocols, junk, reject, spamKeep, allKeep);
      notice = t('plans.saved', { name: saved.name });
      name = '';
      quota = '';
      maxMessage = '';
      perHour = '';
      junkScore = '';
      rejectScore = '';
      spamDays = '';
      allDays = '';
      protoImap = protoPop3 = protoSubmission = true;
      await load.run();
    } catch (err) {
      error = err;
    } finally {
      busy = false;
    }
  }

  const limit = (n: number | null): string => (n === null ? t('plans.none') : bytes(n));
</script>

<main id="content" tabindex="-1">
  <PageHead title={t('plans.title')} lede={t('plans.lede')} />
  {#if error !== null}<ErrorNote {error} />{/if}
  {#if load.state.status === 'error'}<ErrorNote error={load.state.error} />{/if}
  {#if invalid}<p class="alert danger" role="alert">{t('plans.invalid')}</p>{/if}
  {#if notice !== null}<p class="alert ok" role="status">{notice}</p>{/if}
  {#if load.state.status === 'loading'}
    <p class="muted" role="status">{t('app.loading')}</p>
  {:else if load.state.status === 'ready'}
    {#if load.state.data.length === 0}
      <div class="empty"><p>{t('plans.empty')}</p></div>
    {:else}
      <div class="table-wrap">
        <table use:labelled aria-label={t('plans.title')}>
          <thead>
            <tr><th scope="col">{t('plans.col.name')}</th><th scope="col">{t('plans.col.quota')}</th><th scope="col">{t('plans.col.maxMessage')}</th><th scope="col">{t('plans.col.perHour')}</th><th scope="col">{t('plans.col.protocols')}</th><th scope="col" class="col-actions">{t('plans.col.action')}</th></tr>
          </thead>
          <tbody>
            {#each load.state.data as p (p.name)}
              <tr>
                <th scope="row">{p.name}</th>
                <td>{limit(p.quota_bytes)}</td>
                <td>{limit(p.max_message_bytes)}</td>
                <td>{p.messages_per_hour === null ? t('plans.none') : p.messages_per_hour}</td>
                <td>{p.protocols === null ? t('plans.allProtocols') : p.protocols.join(', ')}</td>
                <td class="col-actions"><button type="button" class="secondary" onclick={() => edit(p)}>{t('plans.edit')}</button></td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    {/if}
    <section class="panel" aria-labelledby="plan-form">
      <h2 id="plan-form">{t('plans.form')}</h2>
      <form onsubmit={save}>
        <div class="field-row">
          <label>{t('plans.col.name')}<input bind:value={name} name="name" type="text" autocapitalize="none" spellcheck="false" required /></label>
          <label>{t('plans.quotaMb')}<input bind:value={quota} name="quota" type="text" inputmode="decimal" /></label>
          <label>{t('plans.maxMessageMb')}<input bind:value={maxMessage} name="max" type="text" inputmode="decimal" /></label>
          <label>{t('plans.perHour')}<input bind:value={perHour} name="perhour" type="text" inputmode="numeric" /></label>
          <label>{t('plans.junkScore')}<input bind:value={junkScore} name="junk" type="text" inputmode="decimal" /></label>
          <label>{t('plans.rejectScore')}<input bind:value={rejectScore} name="reject" type="text" inputmode="decimal" /></label>
          <label>{t('plans.spamDays')}<input bind:value={spamDays} name="spamdays" type="text" inputmode="numeric" /></label>
          <label>{t('plans.allDays')}<input bind:value={allDays} name="alldays" type="text" inputmode="numeric" /></label>
          <button type="submit" class="primary" disabled={busy}>{busy ? t('common.saving') : t('common.save')}</button>
        </div>
        <fieldset class="checks">
          <legend>{t('plans.protocols')}</legend>
          <label><input type="checkbox" bind:checked={protoImap} /> {t('plans.imap')}</label>
          <label><input type="checkbox" bind:checked={protoPop3} /> {t('plans.pop3')}</label>
          <label><input type="checkbox" bind:checked={protoSubmission} /> {t('plans.submission')}</label>
        </fieldset>
        <p class="muted">{t('plans.note')}</p>
      </form>
    </section>
  {/if}
</main>
