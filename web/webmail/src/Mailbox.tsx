import { createEffect, createResource, createSignal, For, Show, type Component } from 'solid-js';
import type { Api, Folder, Item, Message } from './api';
import { Compose, type Start } from './Compose';
import { errorText } from './errors';
import { size, when } from './format';
import { t, type MessageKey } from './i18n';
import { Outbox } from './Outbox';

/** The outbox is not a folder of the server; it is listed next to them. */
const OUTBOX = -1;

const ROLE_NAMES: Record<string, MessageKey> = { inbox: 'folder.inbox', sent: 'folder.sent', drafts: 'folder.drafts', trash: 'folder.trash', spam: 'folder.spam', archive: 'folder.archive' };

/** The name of a folder: a system folder has a name of the catalogue, any other its own. */
function folderName(f: Folder): string {
  const key = ROLE_NAMES[f.role];
  return key === undefined ? f.name : t(key);
}

/** The signed-in page: folders on the left, the list of the open folder, the open message or the outbox on the right, and the composer over them. */
export const Mailbox: Component<{ api: Api; address: string; onSignOut: () => void }> = (props) => {
  const [folders, { refetch: refetchFolders }] = createResource(() => props.api.folders());
  const [current, setCurrent] = createSignal<number | null>(null);
  const [items, setItems] = createSignal<Item[]>([]);
  const [next, setNext] = createSignal<number | null>(null);
  const [opened, setOpened] = createSignal<Message | null>(null);
  const [composing, setComposing] = createSignal<Start | null>(null);
  const [notice, setNotice] = createSignal('');
  const [error, setError] = createSignal('');
  // Whether the formatted (HTML) version of the open message is shown; a message with no plain text starts with it.
  const [formatted, setFormatted] = createSignal(false);

  const guard = async (work: () => Promise<void>) => {
    setError('');
    try {
      await work();
    } catch (e) {
      setError(errorText(e));
    }
  };

  const open = (folder: number) =>
    guard(async () => {
      setCurrent(folder);
      setOpened(null);
      if (folder === OUTBOX) return;
      const page = await props.api.messages(folder);
      setItems(page.items);
      setNext(page.next);
    });
  const more = () =>
    guard(async () => {
      const f = current();
      const n = next();
      if (f === null || n === null) return;
      const page = await props.api.messages(f, n);
      setItems([...items(), ...page.items]);
      setNext(page.next);
    });
  const read = (id: number) =>
    guard(async () => {
      const m = await props.api.message(id);
      setOpened(m);
      setFormatted(m.has_html && m.text === '');
      if (!m.flags.seen) {
        await props.api.actions([id], 'read');
        setItems(items().map((i) => (i.id === id ? { ...i, flags: { ...i.flags, seen: true } } : i)));
        await refetchFolders();
      }
    });
  const act = (action: 'read' | 'unread' | 'flag' | 'unflag' | 'delete' | 'move', folder?: number) =>
    guard(async () => {
      const m = opened();
      if (m === null) return;
      await props.api.actions([m.id], action, folder);
      if (action === 'delete' || action === 'move') {
        setItems(items().filter((i) => i.id !== m.id));
        setOpened(null);
      } else {
        setOpened({ ...m, flags: { ...m.flags, seen: action === 'unread' ? false : action === 'read' ? true : m.flags.seen, flagged: action === 'flag' ? true : action === 'unflag' ? false : m.flags.flagged } });
        setItems(items().map((i) => (i.id === m.id ? { ...i, flags: { ...i.flags, seen: action === 'unread' ? false : action === 'read' ? true : i.flags.seen, flagged: action === 'flag' ? true : action === 'unflag' ? false : i.flags.flagged } } : i)));
      }
      await refetchFolders();
    });
  const reply = () => {
    const m = opened();
    if (m === null) return;
    const quoted = m.text
      .split('\n')
      .map((l) => `> ${l}`)
      .join('\n');
    setComposing({ to: m.from, subject: m.subject.toLowerCase().startsWith('re:') ? m.subject : t('compose.replyPrefix') + m.subject, text: `\n\n${t('compose.quote', { from: m.from })}\n${quoted}\n`, inReplyTo: m.message_id });
  };
  const trash = () => (folders() ?? []).find((f) => f.role === 'trash');
  const emptyTrash = () =>
    guard(async () => {
      const f = trash();
      if (f === undefined) return;
      await props.api.emptyFolder(f.id);
      await refetchFolders();
      if (current() === f.id) await open(f.id);
    });

  // The Inbox opens first.
  createEffect(() => {
    const f = folders();
    if (f !== undefined && current() === null) {
      const inbox = f.find((x) => x.role === 'inbox') ?? f[0];
      if (inbox !== undefined) void open(inbox.id);
    }
  });

  return (
    <div class="mailbox">
      <header>
        <strong>{t('app.name')}</strong>
        <span class="who">{props.address}</span>
        <button type="button" class="primary" onClick={() => setComposing({ to: '', subject: '', text: '' })}>
          {t('compose.new')}
        </button>
        <button type="button" onClick={props.onSignOut}>
          {t('signout')}
        </button>
      </header>
      <Show when={notice() !== ''}>
        <p class="notice" role="status">
          {notice()}
        </p>
      </Show>
      <Show when={error() !== ''}>
        <p class="error" role="alert">
          {error()}
        </p>
      </Show>
      <nav aria-label={t('folders')}>
        <For each={folders()}>
          {(f) => (
            <button type="button" class="folder" classList={{ active: current() === f.id }} aria-current={current() === f.id ? 'page' : undefined} onClick={() => void open(f.id)}>
              <span>{folderName(f)}</span>
              <Show when={f.unseen > 0}>
                <span class="count">{f.unseen}</span>
              </Show>
            </button>
          )}
        </For>
        <button type="button" class="folder" classList={{ active: current() === OUTBOX }} aria-current={current() === OUTBOX ? 'page' : undefined} onClick={() => void open(OUTBOX)}>
          <span>{t('folder.outbox')}</span>
        </button>
        <Show when={trash() !== undefined}>
          <button type="button" class="quiet" onClick={() => void emptyTrash()}>
            {t('action.emptyTrash')}
          </button>
        </Show>
      </nav>
      <Show when={current() !== OUTBOX} fallback={<Outbox api={props.api} />}>
        <section class="list" aria-label={t('folders')}>
          <Show when={items().length > 0} fallback={<p class="empty">{t('list.empty')}</p>}>
            <ul>
              <For each={items()}>
                {(i) => (
                  <li>
                    <button type="button" class="item" classList={{ unread: !i.flags.seen, active: opened()?.id === i.id }} onClick={() => void read(i.id)}>
                      <span class="line">
                        <span class="from">{i.from}</span>
                        <span class="when">{when(i.date)}</span>
                      </span>
                      <span class="subject">{i.subject === '' ? t('list.nosubject') : i.subject}</span>
                      <Show when={i.flags.flagged}>
                        <span class="flag">{t('list.flagged')}</span>
                      </Show>
                    </button>
                  </li>
                )}
              </For>
            </ul>
            <Show when={next() !== null}>
              <button type="button" class="quiet" onClick={() => void more()}>
                {t('list.more')}
              </button>
            </Show>
          </Show>
        </section>
        <article class="reader">
          <Show when={opened()} fallback={<p class="empty">{t('message.select')}</p>}>
            {(m) => (
              <>
                <h2>{m().subject === '' ? t('list.nosubject') : m().subject}</h2>
                <dl>
                  <dt>{t('message.from')}</dt>
                  <dd>{m().from}</dd>
                  <dt>{t('message.to')}</dt>
                  <dd>{m().to}</dd>
                  <Show when={m().cc !== ''}>
                    <dt>{t('message.cc')}</dt>
                    <dd>{m().cc}</dd>
                  </Show>
                </dl>
                <div class="actions">
                  <button type="button" onClick={reply}>
                    {t('action.reply')}
                  </button>
                  <button type="button" onClick={() => void act(m().flags.seen ? 'unread' : 'read')}>
                    {m().flags.seen ? t('action.unread') : t('action.flag')}
                  </button>
                  <button type="button" onClick={() => void act(m().flags.flagged ? 'unflag' : 'flag')}>
                    {m().flags.flagged ? t('action.unflag') : t('action.flag')}
                  </button>
                  <select aria-label={t('action.move')} onChange={(e) => { const v = Number(e.currentTarget.value); e.currentTarget.value = ''; if (v > 0) void act('move', v); }}>
                    <option value="">{t('action.move')}</option>
                    <For each={(folders() ?? []).filter((f) => f.id !== m().folder)}>{(f) => <option value={f.id}>{folderName(f)}</option>}</For>
                  </select>
                  <button type="button" class="danger" onClick={() => void act('delete')}>
                    {t('action.delete')}
                  </button>
                  <a href={props.api.rawUrl(m().id)} download="">
                    {t('message.original')}
                  </a>
                </div>
                <Show when={m().has_html}>
                  <button type="button" class="quiet" onClick={() => setFormatted(!formatted())}>
                    {formatted() ? t('message.showText') : t('message.showHtml')}
                  </button>
                </Show>
                <Show when={formatted() && m().has_html} fallback={<pre class="text">{m().text}</pre>}>
                  <p class="note">{t('message.remoteBlocked')}</p>
                  <iframe class="formatted" title={t('message.htmlTitle')} sandbox="allow-popups allow-popups-to-escape-sandbox" referrerpolicy="no-referrer" src={props.api.htmlUrl(m().id)} />
                </Show>
                <Show when={m().text_truncated}>
                  <p class="note">{t('message.truncated')}</p>
                </Show>
                <Show when={m().attachments.length > 0}>
                  <h3>{t('message.attachments')}</h3>
                  <ul class="attachments">
                    <For each={m().attachments}>
                      {(a) => (
                        <li>
                          {a.name ?? a.content_type} <span class="when">{size(a.size)}</span>
                        </li>
                      )}
                    </For>
                  </ul>
                </Show>
              </>
            )}
          </Show>
        </article>
      </Show>
      <Show when={composing()}>
        {(start) => (
          <div class="overlay">
            <Compose
              api={props.api}
              start={start()}
              onClose={() => setComposing(null)}
              onSent={() => {
                setComposing(null);
                setNotice(t('compose.queued'));
                setTimeout(() => setNotice(''), 6000);
              }}
            />
          </div>
        )}
      </Show>
    </div>
  );
};
