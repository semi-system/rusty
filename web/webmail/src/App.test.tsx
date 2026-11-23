import { cleanup, fireEvent, render, screen, waitFor } from '@solidjs/testing-library';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { ApiError, type Api, type Folder, type Item, type Message } from './api';
import { App } from './App';
import { en } from './i18n/en';
import { pseudo } from './i18n';
import { setCatalogue } from './i18n';

afterEach(() => {
  cleanup();
  setCatalogue(en, 'en');
});

const folders: Folder[] = [
  { id: 1, name: 'Inbox', role: 'inbox', parent: null, messages: 2, unseen: 1 },
  { id: 4, name: 'Trash', role: 'trash', parent: null, messages: 0, unseen: 0 },
];
const item = (id: number, subject: string, seen: boolean): Item => ({ id, uid: id, subject, from: 'bob@one.test', date: 1_700_000_000, size: 100, flags: { seen, flagged: false, answered: false, draft: false } });
const message: Message = {
  id: 2,
  folder: 1,
  date: 1_700_000_000,
  size: 100,
  flags: { seen: false, flagged: false, answered: false, draft: false },
  from: 'bob@one.test',
  to: 'alice@one.test',
  cc: '',
  subject: 'Lunch',
  message_id: '<m1@one.test>',
  text: 'Noon?\nBring cash.',
  text_truncated: false,
  html_only: false,
  has_html: false,
  attachments: [],
};

function api(over: Partial<Api> = {}): Api {
  return {
    me: vi.fn().mockRejectedValue(new ApiError(401, 'unauthorized')),
    signIn: vi.fn().mockResolvedValue({ address: 'alice@one.test', csrf: 'tok' }),
    signOut: vi.fn().mockResolvedValue(undefined),
    folders: vi.fn().mockResolvedValue(folders),
    messages: vi.fn().mockResolvedValue({ items: [item(2, 'Lunch', false), item(1, 'Hello', true)], next: null }),
    message: vi.fn().mockResolvedValue(message),
    actions: vi.fn().mockResolvedValue({ applied: 1, missing: [] }),
    emptyFolder: vi.fn().mockResolvedValue(undefined),
    send: vi.fn().mockResolvedValue({ id: 9 }),
    outbox: vi.fn().mockResolvedValue([]),
    withdraw: vi.fn().mockResolvedValue(undefined),
    rawUrl: (id: number) => `/api/v1/messages/${id}/raw`,
    htmlUrl: (id: number) => `/api/v1/messages/${id}/html`,
    ...over,
  };
}

describe('the webmail page', () => {
  it('shows the sign-in form without a session, and a wrong password as a message of the catalogue', async () => {
    const a = api({ signIn: vi.fn().mockRejectedValue(new ApiError(401, 'unauthorized')) });
    render(() => <App api={a} />);
    const address = await screen.findByLabelText(en['signin.address']);
    fireEvent.input(address, { target: { value: 'alice@one.test' } });
    fireEvent.input(screen.getByLabelText(en['signin.password']), { target: { value: 'nope' } });
    fireEvent.submit(address.closest('form') as HTMLFormElement);
    expect((await screen.findByRole('alert')).textContent).toBe(en['error.unauthorized']);
    expect(a.signIn).toHaveBeenCalledWith('alice@one.test', 'nope');
  });

  it('opens the Inbox of a signed-in person, reads a message and marks it as read', async () => {
    const a = api({ me: vi.fn().mockResolvedValue({ address: 'alice@one.test', csrf: 'tok' }) });
    render(() => <App api={a} />);
    fireEvent.click(await screen.findByText('Lunch'));
    expect(await screen.findByText(/Bring cash/)).toBeTruthy();
    await waitFor(() => expect(a.actions).toHaveBeenCalledWith([2], 'read'));
    expect(a.messages).toHaveBeenCalledWith(1);
  });

  it('writes a reply to the sender with the message it answers, and says the message is queued', async () => {
    const a = api({ me: vi.fn().mockResolvedValue({ address: 'alice@one.test', csrf: 'tok' }) });
    render(() => <App api={a} />);
    fireEvent.click(await screen.findByText('Lunch'));
    fireEvent.click(await screen.findByText(en['action.reply']));
    const to = (await screen.findByLabelText(en['compose.to'])) as HTMLInputElement;
    expect(to.value).toBe('bob@one.test');
    expect((screen.getByLabelText(en['compose.subject']) as HTMLInputElement).value).toBe('Re: Lunch');
    fireEvent.click(screen.getByText(en['compose.send']));
    await waitFor(() => expect(a.send).toHaveBeenCalled());
    const sent = (a.send as ReturnType<typeof vi.fn>).mock.calls[0]?.[0] as { to: string[]; in_reply_to: string; subject: string };
    expect(sent.to).toEqual(['bob@one.test']);
    expect(sent.in_reply_to).toBe('<m1@one.test>');
    expect((await screen.findByRole('status')).textContent).toBe(en['compose.queued']);
  });

  it('a refused send keeps the form and says why', async () => {
    const a = api({ me: vi.fn().mockResolvedValue({ address: 'alice@one.test', csrf: 'tok' }), send: vi.fn().mockRejectedValue(new ApiError(400, 'bad_address')) });
    render(() => <App api={a} />);
    fireEvent.click(await screen.findByText(en['compose.new']));
    fireEvent.click(await screen.findByText(en['compose.send']));
    expect((await screen.findByRole('alert')).textContent).toBe(en['error.bad_address']);
    expect(screen.getByLabelText(en['compose.to'])).toBeTruthy();
  });

  it('a message with only HTML is shown in a frame that cannot run script, and can be switched to plain text when it has some', async () => {
    const html = { ...message, text: '', html_only: true, has_html: true };
    const a = api({ me: vi.fn().mockResolvedValue({ address: 'alice@one.test', csrf: 'tok' }), message: vi.fn().mockResolvedValue(html) });
    render(() => <App api={a} />);
    fireEvent.click(await screen.findByText('Lunch'));
    const frame = (await screen.findByTitle(en['message.htmlTitle'])) as HTMLIFrameElement;
    expect(frame.getAttribute('sandbox')).toBe('allow-popups allow-popups-to-escape-sandbox');
    expect(frame.getAttribute('src')).toBe('/api/v1/messages/2/html');
    expect(frame.getAttribute('referrerpolicy')).toBe('no-referrer');
  });

  it('deleting removes the message from the list', async () => {
    const a = api({ me: vi.fn().mockResolvedValue({ address: 'alice@one.test', csrf: 'tok' }) });
    render(() => <App api={a} />);
    fireEvent.click(await screen.findByText('Lunch'));
    fireEvent.click(await screen.findByText(en['action.delete']));
    await waitFor(() => expect(a.actions).toHaveBeenCalledWith([2], 'delete', undefined));
    await waitFor(() => expect(screen.queryByText('Lunch')).toBeNull());
  });

  it('every text of the page comes from the catalogue: in the pseudo-locale nothing plain is left', async () => {
    setCatalogue(pseudo(en), 'xx');
    const a = api({ me: vi.fn().mockResolvedValue({ address: 'alice@one.test', csrf: 'tok' }) });
    render(() => <App api={a} />);
    fireEvent.click(await screen.findByText('Lunch'));
    await screen.findByText(/Bring cash/);
    const texts = Array.from(document.querySelectorAll('header button, .actions button, h2, dt, label span, option')).map((e) => (e.textContent ?? '').trim()).filter((s) => s !== '');
    for (const s of texts) {
      // The subject comes from the server; every other text carries the catalogue's mark.
      if (s === 'Lunch') continue;
      expect(s, s).toMatch(/~~\]$/);
    }
  });
});
