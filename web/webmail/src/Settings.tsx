import { createResource, createSignal, For, Show, type Component } from 'solid-js';
import type { Api } from './api';
import { errorText } from './errors';
import { when } from './format';
import { t } from './i18n';

/** The app passwords of the account: one per device or program, shown once when made, revoked one by one. The mail protocols accept them; this page never does. */
export const Settings: Component<{ api: Api; onSignature: () => void }> = (props) => {
  const [signature, setSignature] = createSignal('');
  const [signatureSaved, setSignatureSaved] = createSignal(false);
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
