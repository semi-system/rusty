import { createResource, createSignal, Show, type Component } from 'solid-js';
import type { Api } from './api';
import { errorText } from './errors';
import { t } from './i18n';

/** Two-step sign-in (docs/design-2fa.md): begin with the password, add the key to an authenticator app, confirm with its first code, keep the recovery codes; turn off with the password and a code. Not shown when the server has no key for it. */
export const TwoStep: Component<{ api: Api }> = (props) => {
  const [status, { refetch }] = createResource(async () => {
    try {
      return await props.api.totpStatus();
    } catch {
      return { available: false, enabled: false, pending: false };
    }
  });
  const [password, setPassword] = createSignal('');
  const [code, setCode] = createSignal('');
  const [key, setKey] = createSignal<{ secret: string; uri: string } | null>(null);
  const [recovery, setRecovery] = createSignal<string[]>([]);
  const [error, setError] = createSignal('');
  const step = (work: () => Promise<void>) => async (e: SubmitEvent) => {
    e.preventDefault();
    setError('');
    try {
      await work();
      await refetch();
    } catch (err) {
      setError(errorText(err));
    }
  };
  const begin = step(async () => {
    setKey(await props.api.totpBegin(password()));
    setPassword('');
  });
  const confirm = step(async () => {
    setRecovery((await props.api.totpConfirm(code())).recovery);
    setKey(null);
    setCode('');
  });
  const disable = step(async () => {
    await props.api.totpDisable(password(), code());
    setPassword('');
    setCode('');
  });
  const passwordField = (
    <label>
      <span>{t('settings.apppw.confirm')}</span>
      <input type="password" name="totp-password" autocomplete="current-password" required value={password()} onInput={(e) => setPassword(e.currentTarget.value)} />
    </label>
  );
  const codeField = (
    <label>
      <span>{t('settings.totp.code')}</span>
      <input name="totp-code" autocomplete="one-time-code" required value={code()} onInput={(e) => setCode(e.currentTarget.value)} />
    </label>
  );
  return (
    <Show when={status()?.available}>
      <h2>{t('settings.totp.title')}</h2>
      <p class="note">{t('settings.totp.about')}</p>
      <Show when={recovery().length > 0}>
        <div class="secret" role="status">
          <p>{t('settings.totp.recovery')}</p>
          <code>{recovery().join('  ')}</code>
          <button type="button" onClick={() => setRecovery([])}>
            {t('settings.apppw.done')}
          </button>
        </div>
      </Show>
      <Show when={error() !== ''}>
        <p class="error" role="alert">
          {error()}
        </p>
      </Show>
      <Show
        when={status()?.enabled}
        fallback={
          <Show
            when={key()}
            fallback={
              <form onSubmit={begin}>
                {passwordField}
                <button type="submit">{t('settings.totp.begin')}</button>
              </form>
            }
          >
            {(k) => (
              <form onSubmit={confirm}>
                <p>{t('settings.totp.scan')}</p>
                <code class="secretline">{k().secret}</code>
                <a href={k().uri}>{t('settings.totp.open')}</a>
                {codeField}
                <button type="submit" class="primary">
                  {t('settings.totp.confirm')}
                </button>
              </form>
            )}
          </Show>
        }
      >
        <form onSubmit={disable}>
          <p>{t('settings.totp.on')}</p>
          {passwordField}
          {codeField}
          <button type="submit" class="danger">
            {t('settings.totp.disable')}
          </button>
        </form>
      </Show>
    </Show>
  );
};
