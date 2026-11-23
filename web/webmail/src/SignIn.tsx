import { createSignal, Show, type Component } from 'solid-js';
import { ApiError, type Api, type Me } from './api';
import { errorText } from './errors';
import { t } from './i18n';

export const SignIn: Component<{ api: Api; onSignedIn: (m: Me) => void }> = (props) => {
  const [address, setAddress] = createSignal('');
  const [password, setPassword] = createSignal('');
  const [code, setCode] = createSignal('');
  // The account has a second factor: the form asks for the code next to the password.
  const [needCode, setNeedCode] = createSignal(false);
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal('');
  const submit = async (e: SubmitEvent) => {
    e.preventDefault();
    setBusy(true);
    setError('');
    const sent = code().trim();
    try {
      props.onSignedIn(await props.api.signIn(address().trim(), password(), needCode() ? sent : undefined));
    } catch (err) {
      if (err instanceof ApiError && err.code === 'second_factor') {
        // Asking for the code is the next step; a code that was refused is said.
        if (needCode() && sent !== '') setError(errorText(err));
        setNeedCode(true);
        setCode('');
      } else {
        setError(errorText(err));
        setPassword('');
        setNeedCode(false);
      }
    } finally {
      setBusy(false);
    }
  };
  return (
    <main class="signin">
      <h1>{t('app.name')}</h1>
      <form onSubmit={submit}>
        <h2>{t('signin.title')}</h2>
        <label>
          <span>{t('signin.address')}</span>
          <input type="email" name="address" autocomplete="username" required value={address()} onInput={(e) => setAddress(e.currentTarget.value)} />
        </label>
        <label>
          <span>{t('signin.password')}</span>
          <input type="password" name="password" autocomplete="current-password" required value={password()} onInput={(e) => setPassword(e.currentTarget.value)} />
        </label>
        <Show when={needCode()}>
          <label>
            <span>{t('signin.code')}</span>
            <input name="code" autocomplete="one-time-code" inputmode="numeric" required value={code()} onInput={(e) => setCode(e.currentTarget.value)} />
          </label>
        </Show>
        <Show when={error() !== ''}>
          <p class="error" role="alert">
            {error()}
          </p>
        </Show>
        <button type="submit" class="primary" disabled={busy()}>
          {busy() ? t('signin.busy') : t('signin.submit')}
        </button>
      </form>
    </main>
  );
};
