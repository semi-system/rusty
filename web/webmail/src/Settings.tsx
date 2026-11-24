import { createResource, createSignal, For, Show, type Component } from 'solid-js';
import type { Api } from './api';
import { errorText } from './errors';
import { cardBatches, when } from './format';
import { t } from './i18n';
import { Filters } from './Filters';
import { TwoStep } from './TwoStep';

/** The app passwords of the account: one per device or program, shown once when made, revoked one by one. The mail protocols accept them; this page never does. */
export const Settings: Component<{ api: Api; onSignature: () => void }> = (props) => {
  const [signature, setSignature] = createSignal('');
  const [signatureSaved, setSignatureSaved] = createSignal(false);
  const [imported, setImported] = createSignal('');
  const [importError, setImportError] = createSignal('');
  const importFile = async (file: File | undefined) => {
    setImported('');
    setImportError('');
    if (file === undefined) return;
    try {
      const sum = { imported: 0, updated: 0, invalid: 0, duplicates: 0 };
      for (const batch of cardBatches(await file.text())) {
        const r = await props.api.importContacts(batch);
        sum.imported += r.imported;
        sum.updated += r.updated;
        sum.invalid += r.invalid;
        sum.duplicates += r.duplicates;
      }
      setImported(t('settings.contacts.imported', { added: sum.imported, updated: sum.updated, skipped: sum.invalid + sum.duplicates }));
    } catch (err) {
      setImportError(errorText(err));
    }
  };
  const [loaded] = createResource(async () => {
    try {
      setSignature((await props.api.settings()).signature);
    } catch {
      setSignature('');
    }
    return true;
  });
  const saveSignature = async (e: SubmitEvent) => {
    e.preventDefault();
    setError('');
    setSignatureSaved(false);
    try {
      setSignature((await props.api.saveSettings(signature())).signature);
      setSignatureSaved(true);
      props.onSignature();
    } catch (err) {
      setError(errorText(err));
    }
  };
  const [items, { refetch }] = createResource(() => props.api.appPasswords());
  const [label, setLabel] = createSignal('');
  const [password, setPassword] = createSignal('');
  const [secret, setSecret] = createSignal('');
  const [error, setError] = createSignal('');
  const [busy, setBusy] = createSignal(false);
  const make = async (e: SubmitEvent) => {
    e.preventDefault();
    setBusy(true);
    setError('');
    try {
      const made = await props.api.createAppPassword(label().trim(), password());
      setSecret(made.secret);
      setLabel('');
      setPassword('');
      await refetch();
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  };
  const revoke = async (id: number) => {
    setError('');
    try {
      await props.api.revokeAppPassword(id);
      await refetch();
    } catch (err) {
      setError(errorText(err));
    }
  };
  return (
    <section class="settings" aria-label={t('settings.title')}>
      <h2>{t('settings.signature.title')}</h2>
      <form onSubmit={saveSignature}>
        <label>
          <span>{t('settings.signature.label')}</span>
          <textarea name="signature" rows="4" maxlength="2000" disabled={!loaded()} value={signature()} onInput={(e) => setSignature(e.currentTarget.value)} />
        </label>
        <button type="submit">{t('settings.signature.save')}</button>
        <Show when={signatureSaved()}>
          <span class="note" role="status">{t('settings.signature.saved')}</span>
        </Show>
      </form>
      <h2>{t('settings.contacts.title')}</h2>
      <p class="note">{t('settings.contacts.about')}</p>
      <p>
        <a href="/api/v1/contacts/export" download="contacts.vcf">
          {t('settings.contacts.export')}
        </a>
      </p>
      <label>
        <span>{t('settings.contacts.import')}</span>
        <input type="file" accept=".vcf,text/vcard" onChange={(e) => void importFile(e.currentTarget.files?.[0])} />
      </label>
      <Show when={imported() !== ''}>
        <p class="note" role="status">
          {imported()}
        </p>
      </Show>
      <Show when={importError() !== ''}>
        <p class="error" role="alert">
          {importError()}
        </p>
      </Show>
      <Filters api={props.api} />
      <TwoStep api={props.api} />
      <h2>{t('settings.apppw.title')}</h2>
      <p class="note">{t('settings.apppw.about')}</p>
      <Show when={secret() !== ''}>
        <div class="secret" role="status">
          <p>{t('settings.apppw.once')}</p>
          <code>{secret()}</code>
          <button type="button" onClick={() => setSecret('')}>
            {t('settings.apppw.done')}
          </button>
        </div>
      </Show>
      <form onSubmit={make}>
        <label>
          <span>{t('settings.apppw.label')}</span>
          <input name="label" maxlength="60" required value={label()} onInput={(e) => setLabel(e.currentTarget.value)} />
        </label>
        <label>
          <span>{t('settings.apppw.confirm')}</span>
          <input type="password" name="password" autocomplete="current-password" required value={password()} onInput={(e) => setPassword(e.currentTarget.value)} />
        </label>
        <Show when={error() !== ''}>
          <p class="error" role="alert">
            {error()}
          </p>
        </Show>
        <button type="submit" class="primary" disabled={busy()}>
          {t('settings.apppw.make')}
        </button>
      </form>
      <Show when={(items() ?? []).length > 0} fallback={<p class="empty">{t('settings.apppw.none')}</p>}>
        <ul class="apppw">
          <For each={items()}>
            {(i) => (
              <li>
                <div class="line">
                  <strong>{i.label}</strong>
                  <span class="when">{i.last_used === null ? t('settings.apppw.unused') : t('settings.apppw.used', { when: when(i.last_used) })}</span>
                </div>
                <button type="button" class="danger" onClick={() => void revoke(i.id)}>
                  {t('settings.apppw.revoke')}
                </button>
              </li>
            )}
          </For>
        </ul>
      </Show>
    </section>
  );
};
