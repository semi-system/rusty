import { createSignal, Show, type Component } from 'solid-js';
import { addressesOf, type Api } from './api';
import { errorText } from './errors';
import { t } from './i18n';

/** What the form starts with (a new message is all empty; a reply names the sender, the subject and the message it answers; a draft brings everything back). */
export interface Start {
  to: string;
  subject: string;
  text: string;
  cc?: string;
  bcc?: string;
  inReplyTo?: string;
  /** The draft being edited, if any. */
  draftId?: number;
}

export const Compose: Component<{ api: Api; start: Start; onClose: () => void; onSent: () => void }> = (props) => {
  const [to, setTo] = createSignal(props.start.to);
  const [cc, setCc] = createSignal(props.start.cc ?? '');
  const [bcc, setBcc] = createSignal(props.start.bcc ?? '');
  const [subject, setSubject] = createSignal(props.start.subject);
  const [text, setText] = createSignal(props.start.text);
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal('');
  const [draftId, setDraftId] = createSignal<number | undefined>(props.start.draftId);
  const [saved, setSaved] = createSignal(false);
  const fields = () => ({
    to: addressesOf(to()),
    cc: addressesOf(cc()),
    bcc: addressesOf(bcc()),
    subject: subject(),
    text: text(),
    ...(props.start.inReplyTo ? { in_reply_to: props.start.inReplyTo } : {}),
  });
  const save = async () => {
    setBusy(true);
    setError('');
    setSaved(false);
    try {
      const kept = await props.api.saveDraft(fields(), draftId());
      setDraftId(kept.id);
      setSaved(true);
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  };
  const submit = async (e: SubmitEvent) => {
    e.preventDefault();
    setBusy(true);
    setError('');
    try {
      const id = draftId();
      await props.api.send({ ...fields(), ...(id === undefined ? {} : { draft_id: id }) });
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
      <Show when={saved()}>
        <p class="note" role="status">
          {t('compose.saved')}
        </p>
      </Show>
      <div class="row">
        <button type="submit" class="primary" disabled={busy()}>
          {busy() ? t('compose.sending') : t('compose.send')}
        </button>
        <button type="button" disabled={busy()} onClick={() => void save()}>
          {t('compose.save')}
        </button>
        <button type="button" onClick={props.onClose}>
          {t('compose.cancel')}
        </button>
      </div>
    </form>
  );
};
