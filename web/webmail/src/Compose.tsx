import { createSignal, For, onCleanup, Show, type Component } from 'solid-js';
import { addressesOf, type Api, type Contact } from './api';
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

/** The part of an address field the person is typing now: what follows the last comma or semicolon. */
function lastPart(value: string): string {
  return value.split(/[,;]/).pop()?.trim() ?? '';
}

/** An address field that offers the contacts of the person while they type (two characters or more, after a short pause). A suggestion puts its address in place of the part being typed. */
const AddressField: Component<{ api: Api; name: string; label: string; value: string; onValue: (v: string) => void }> = (props) => {
  const [items, setItems] = createSignal<Contact[]>([]);
  let timer: ReturnType<typeof setTimeout> | undefined;
  let latest = 0;
  onCleanup(() => clearTimeout(timer));
  const ask = (value: string) => {
    clearTimeout(timer);
    const q = lastPart(value);
    const mine = ++latest;
    if (q.length < 2) {
      setItems([]);
      return;
    }
    timer = setTimeout(() => {
      props.api.contacts(q).then(
        (found) => {
          if (mine === latest) setItems(found);
        },
        () => {
          if (mine === latest) setItems([]);
        },
      );
    }, 200);
  };
  const pick = (c: Contact) => {
    const cut = Math.max(props.value.lastIndexOf(','), props.value.lastIndexOf(';')) + 1;
    const head = props.value.slice(0, cut).trimEnd();
    props.onValue(`${head === '' ? '' : `${head} `}${c.email}, `);
    latest++;
    setItems([]);
  };
  return (
    <div class="field">
      <label>
        <span>{props.label}</span>
        <input
          name={props.name}
          autocomplete="off"
          value={props.value}
          onInput={(e) => {
            props.onValue(e.currentTarget.value);
            ask(e.currentTarget.value);
          }}
        />
      </label>
      <Show when={items().length > 0}>
        <ul class="suggestions" aria-label={t('compose.suggestions')}>
          <For each={items()}>
            {(c) => (
              <li>
                <button type="button" onClick={() => pick(c)}>
                  {c.name} &lt;{c.email}&gt;
                </button>
              </li>
            )}
          </For>
        </ul>
      </Show>
    </div>
  );
};

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
      <AddressField api={props.api} name="to" label={t('compose.to')} value={to()} onValue={setTo} />
      <AddressField api={props.api} name="cc" label={t('compose.cc')} value={cc()} onValue={setCc} />
      <AddressField api={props.api} name="bcc" label={t('compose.bcc')} value={bcc()} onValue={setBcc} />
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
