<script lang="ts">
  import { onMount } from 'svelte';
  import type { App } from '../lib/app.svelte';
  import type { DomainSettings, Me, Plan } from '../api/types';
  import DomainShell from '../lib/DomainShell.svelte';
  import ErrorNote from '../lib/ErrorNote.svelte';
  import { megabytes } from '../lib/bytes';
  import { t } from '../i18n';

  let { app, me, name }: { app: App; me: Me; name: string } = $props();

  const platform = $derived(me.scope === 'platform');
  let plans = $state<Plan[]>([]);
  let loaded = $state(false);
  let error = $state<unknown>(null);
  let notice = $state<string | null>(null);
  let invalid = $state(false);
  let busy = $state(false);

  // The form holds text: an empty field means "none".
  let defaultPlan = $state('');
  let maxAccounts = $state('');
  let quotaMb = $state('');
  let maxMessageMb = $state('');
  let minLength = $state('');
  let classes = $state('');
  // One entry a line: an address or a domain.
  let allowText = $state('');
  let blockText = $state('');
  const lines = (text: string): string[] => text.split('\n').map((l) => l.trim()).filter((l) => l !== '');

  const mb = (n: number | null): string => (n === null ? '' : String(n / 1_048_576));
  const whole = (n: number | null): string => (n === null ? '' : String(n));

  async function load(): Promise<void> {
    try {
      const [s, p] = await Promise.all([app.client.domainSettings(name), app.client.plans()]);
      plans = p;
      defaultPlan = s.default_plan ?? '';
      maxAccounts = whole(s.max_accounts);
      quotaMb = mb(s.quota_bytes);
      maxMessageMb = mb(s.max_message_bytes);
      minLength = whole(s.password_min_length);
      classes = whole(s.password_classes);
      allowText = s.sender_allow.join('\n');
      blockText = s.sender_block.join('\n');
      loaded = true;
    } catch (e) {
      error = e;
    }
  }

  onMount(() => void load());

  const count = (text: string): number | null | undefined => {
    const v = text.trim();
    if (v === '') return null;
    const n = Number(v);
    return Number.isInteger(n) && n >= 0 ? n : undefined;
  };

  async function save(e: SubmitEvent): Promise<void> {
    e.preventDefault();
    const accounts = count(maxAccounts);
    const length = count(minLength);
    const kinds = count(classes);
    const quota = megabytes(quotaMb);
    const message = megabytes(maxMessageMb);
    invalid = accounts === undefined || length === undefined || kinds === undefined || Number.isNaN(quota) || Number.isNaN(message);
    if (invalid) return;
    const input: DomainSettings = {
      default_plan: defaultPlan === '' ? null : defaultPlan,
      max_accounts: accounts ?? null,
      quota_bytes: quota,
      max_message_bytes: message,
      password_min_length: length ?? null,
      password_classes: kinds ?? null,
      sender_allow: lines(allowText),
      sender_block: lines(blockText),
    };
    busy = true;
    error = null;
    notice = null;
    try {
      await app.client.setDomainSettings(name, input);
      notice = t('settings.saved');
    } catch (err) {
      error = err;
    } finally {
      busy = false;
    }
  }
</script>

<main id="content" tabindex="-1">
  <DomainShell {name}>
    <div class="section-head">
      <h2>{t('settings.title', { domain: name })}</h2>
      <p>{t('settings.lede')}</p>
    </div>
    {#if error !== null}<ErrorNote {error} />{/if}
    {#if invalid}<p class="alert danger" role="alert">{t('settings.invalid')}</p>{/if}
    {#if notice !== null}<p class="alert ok" role="status">{notice}</p>{/if}
    {#if !loaded}
      <p class="muted" role="status">{t('app.loading')}</p>
    {:else}
      <form onsubmit={save}>
        <section class="panel" aria-labelledby="set-accounts">
          <h2 id="set-accounts">{t('settings.accounts')}</h2>
          {#if !platform}<p class="hint">{t('settings.platformOnly')}</p>{/if}
          <div class="field-row">
            <label>
              {t('settings.defaultPlan')}
              <select bind:value={defaultPlan} disabled={!platform}>
                <option value="">{t('accounts.noPlan')}</option>
                {#each plans as p (p.name)}<option value={p.name}>{p.name}</option>{/each}
              </select>
            </label>
            <label>{t('settings.maxAccounts')}<input bind:value={maxAccounts} name="max" type="text" inputmode="numeric" disabled={!platform} /></label>
          </div>
          <div class="field-row">
            <label>{t('settings.quotaMb')}<input bind:value={quotaMb} name="quota" type="text" inputmode="decimal" disabled={!platform} /></label>
            <label>{t('settings.maxMessageMb')}<input bind:value={maxMessageMb} name="message" type="text" inputmode="decimal" disabled={!platform} /></label>
          </div>
        </section>
        <section class="panel" aria-labelledby="set-passwords">
          <h2 id="set-passwords">{t('settings.passwords')}</h2>
          <p class="hint">{t('settings.passwordsLede')}</p>
          <div class="field-row">
            <label>{t('settings.minLength')}<input bind:value={minLength} name="length" type="text" inputmode="numeric" /></label>
            <label>{t('settings.classes')}<input bind:value={classes} name="classes" type="text" inputmode="numeric" /></label>
          </div>
        </section>
        <section class="panel" aria-labelledby="set-senders">
          <h2 id="set-senders">{t('settings.senders')}</h2>
          <p class="hint">{t('settings.sendersLede')}</p>
          <div class="field-row">
            <label>{t('settings.allow')}<textarea bind:value={allowText} name="allow" rows="5" autocapitalize="none" spellcheck="false"></textarea></label>
            <label>{t('settings.block')}<textarea bind:value={blockText} name="block" rows="5" autocapitalize="none" spellcheck="false"></textarea></label>
          </div>
        </section>
        <div class="form-actions"><button type="submit" class="primary" disabled={busy}>{busy ? t('common.saving') : t('common.save')}</button></div>
      </form>
    {/if}
  </DomainShell>
</main>
