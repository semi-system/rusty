import { createSignal, Show, type Component } from 'solid-js';
import type { Api, Me } from './api';
import { errorText } from './errors';
import { t } from './i18n';

export const SignIn: Component<{ api: Api; onSignedIn: (m: Me) => void }> = (props) => {
  const [address, setAddress] = createSignal('');
  const [password, setPassword] = createSignal('');
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal('');
  const submit = async (e: SubmitEvent) => {
    e.preventDefault();
    setBusy(true);
    setError('');
    try {
      props.onSignedIn(await props.api.signIn(address().trim(), password()));
    } catch (err) {
      setError(errorText(err));
      setPassword('');
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
