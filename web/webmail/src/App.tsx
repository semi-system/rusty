import { createSignal, onMount, Show, type Component } from 'solid-js';
import type { Api, Me } from './api';
import { t } from './i18n';
import { Mailbox } from './Mailbox';
import { SignIn } from './SignIn';

/** The page: signed out shows the sign-in form, signed in shows the mailbox. The session itself is the cookie; the token of the session comes from `me`. */
export const App: Component<{ api: Api }> = (props) => {
  const [me, setMe] = createSignal<Me | null>(null);
  const [ready, setReady] = createSignal(false);
  onMount(async () => {
    try {
      setMe(await props.api.me());
    } catch {
      setMe(null);
    }
    setReady(true);
  });
  const signOut = async () => {
    try {
      await props.api.signOut();
    } finally {
      setMe(null);
    }
  };
  return (
    <Show when={ready()} fallback={<p class="loading">{t('loading')}</p>}>
      <Show when={me()} fallback={<SignIn api={props.api} onSignedIn={setMe} />}>
        {(m) => <Mailbox api={props.api} address={m().address} onSignOut={signOut} />}
      </Show>
    </Show>
  );
};
