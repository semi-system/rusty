import { createSignal, Show, type Component } from 'solid-js';
import { addressesOf, type Api } from './api';
import { errorText } from './errors';
import { t } from './i18n';

/** What the form starts with (a new message is all empty; a reply names the sender, the subject and the message it answers). */
export interface Start {
  to: string;
  subject: string;
  text: string;
  inReplyTo?: string;
}

export const Compose: Component<{ api: Api; start: Start; onClose: () => void; onSent: () => void }> = (props) => {
  const [to, setTo] = createSignal(props.start.to);
  const [cc, setCc] = createSignal('');
  const [bcc, setBcc] = createSignal('');
  const [subject, setSubject] = createSignal(props.start.subject);
  const [text, setText] = createSignal(props.start.text);
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal('');
  const submit = async (e: SubmitEvent) => {
    e.preventDefault();
    setBusy(true);
    setError('');
    try {
      await props.api.send({ to: addressesOf(to()), cc: addressesOf(cc()), bcc: addressesOf(bcc()), subject: subject(), text: text(), ...(props.start.inReplyTo ? { in_reply_to: props.start.inReplyTo } : {}) });
      props.onSent();
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  };
  return (
    <form class="compose" aria-label={t('compose.title')} onSubmit={submit}>
      <h2>{t('compose.title')}</h2>
      <label>
        <span>{t('compose.to')}</span>
        <input name="to" autocomplete="off" value={to()} onInput={(e) => setTo(e.currentTarget.value)} />
      </label>
      <label>
        <span>{t('compose.cc')}</span>
        <input name="cc" autocomplete="off" value={cc()} onInput={(e) => setCc(e.currentTarget.value)} />
      </label>
      <label>
        <span>{t('compose.bcc')}</span>
        <input name="bcc" autocomplete="off" value={bcc()} onInput={(e) => setBcc(e.currentTarget.value)} />
      </label>
      <label>
        <span>{t('compose.subject')}</span>
        <input name="subject" value={subject()} onInput={(e) => setSubject(e.currentTarget.value)} />
      </label>
      <label>
        <span>{t('compose.text')}</span>
        <textarea name="text" rows="12" value={text()} onInput={(e) => setText(e.currentTarget.value)} />
      </label>
      <Show when={error() !== ''}>
        <p class="error" role="alert">
          {error()}
        </p>
      </Show>
      <div class="row">
        <button type="submit" class="primary" disabled={busy()}>
          {busy() ? t('compose.sending') : t('compose.send')}
        </button>
        <button type="button" onClick={props.onClose}>
          {t('compose.cancel')}
        </button>
      </div>
    </form>
  );
};
