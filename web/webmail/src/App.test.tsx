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
  bcc: '',
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
    attachmentUrl: (id: number, n: number) => `/api/v1/messages/${id}/attachments/${n}`,
    saveDraft: vi.fn().mockResolvedValue({ id: 77 }),

    contacts: vi.fn().mockResolvedValue([]),
    events: vi.fn().mockResolvedValue({ items: [], truncated: false }),
    importContacts: vi.fn().mockResolvedValue({ imported: 0, updated: 0, invalid: 0, duplicates: 0 }),
    createEvent: vi.fn().mockResolvedValue(undefined),
    settings: vi.fn().mockResolvedValue({ signature: 'Alice' }),
    saveSettings: vi.fn().mockImplementation(async (s: string) => ({ signature: s })),
    scripts: vi.fn().mockResolvedValue({ scripts: [], max_scripts: 10, max_bytes: 65536 }),
    script: vi.fn().mockResolvedValue({ name: 'main', body: 'keep;' }),
    saveScript: vi.fn().mockResolvedValue(undefined),
    activateScript: vi.fn().mockResolvedValue(undefined),
    deleteScript: vi.fn().mockResolvedValue(undefined),
    totpStatus: vi.fn().mockResolvedValue({ available: true, enabled: false, pending: false }),
    totpBegin: vi.fn().mockResolvedValue({ secret: 'JBSWY3DPEHPK3PXP', uri: 'otpauth://totp/x' }),
    totpConfirm: vi.fn().mockResolvedValue({ recovery: ['abcde-fghjk'] }),
    totpDisable: vi.fn().mockResolvedValue(undefined),
    appPasswords: vi.fn().mockResolvedValue([]),
    createAppPassword: vi.fn().mockResolvedValue({ id: 1, label: 'Phone', secret: 'abcd-efgh-jkmn-pqrs' }),
    revokeAppPassword: vi.fn().mockResolvedValue(undefined),
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
    expect(a.signIn).toHaveBeenCalledWith('alice@one.test', 'nope', undefined);
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

  it('offers the contacts of the person while an address is typed and puts the address of the chosen one in place of the part being typed', async () => {
    const contacts = vi.fn().mockResolvedValue([{ name: 'Ada Lovelace', email: 'ada@example.org' }]);
    const a = api({ me: vi.fn().mockResolvedValue({ address: 'alice@one.test', csrf: 'tok' }), contacts });
    render(() => <App api={a} />);
    fireEvent.click(await screen.findByText(en['compose.new']));
    const to = (await screen.findByLabelText(en['compose.to'])) as HTMLInputElement;
    // One character asks nothing.
    fireEvent.input(to, { target: { value: 'a' } });
    await new Promise((r) => setTimeout(r, 300));
    expect(contacts).not.toHaveBeenCalled();
    // After a comma, the part being typed is what is asked.
    fireEvent.input(to, { target: { value: 'bob@one.test, ad' } });
    const pick = await screen.findByText('Ada Lovelace <ada@example.org>');
    expect(contacts).toHaveBeenCalledWith('ad');
    fireEvent.click(pick);
    expect((screen.getByLabelText(en['compose.to']) as HTMLInputElement).value).toBe('bob@one.test, ada@example.org, ');
    expect(screen.queryByText('Ada Lovelace <ada@example.org>')).toBeNull();
  });

  it('a failing contact search is no error of the form: the field just offers nothing', async () => {
    const contacts = vi.fn().mockRejectedValue(new ApiError(503, 'busy'));
    const a = api({ me: vi.fn().mockResolvedValue({ address: 'alice@one.test', csrf: 'tok' }), contacts });
    render(() => <App api={a} />);
    fireEvent.click(await screen.findByText(en['compose.new']));
    fireEvent.input(await screen.findByLabelText(en['compose.to']), { target: { value: 'ad' } });
    await waitFor(() => expect(contacts).toHaveBeenCalled());
    expect(screen.queryByRole('alert')).toBeNull();
    expect(screen.queryByLabelText(en['compose.suggestions'])).toBeNull();
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

  it('lists the events of the next two weeks by day, a time as the event states it, and says when a rule is not expanded', async () => {
    const day = Date.UTC(2030, 4, 6) / 1000;
    const events = vi.fn().mockResolvedValue({
      items: [
        { uid: 'a', summary: 'Stand-up', location: 'Room 1', start: day + 9 * 3600, end: day + 9.5 * 3600, all_day: false, utc: false, tzid: 'Europe/Berlin', unexpanded: false, calendar: 'default' },
        { uid: 'b', summary: 'Holiday', location: '', start: day + 86400, end: day + 2 * 86400, all_day: true, utc: false, tzid: null, unexpanded: true, calendar: 'default' },
      ],
      truncated: false,
    });
    const a = api({ me: vi.fn().mockResolvedValue({ address: 'alice@one.test', csrf: 'tok' }), events });
    render(() => <App api={a} />);
    fireEvent.click(await screen.findByText(en['folder.agenda']));
    expect(await screen.findByText('Stand-up')).toBeTruthy();
    expect(screen.getByText('09:00')).toBeTruthy();
    expect(screen.getByText('Europe/Berlin')).toBeTruthy();
    expect(screen.getByText(en['agenda.allday'])).toBeTruthy();
    expect(screen.getByText(en['agenda.repeats'])).toBeTruthy();
    expect(events).toHaveBeenCalledTimes(1);
    const [from, to] = events.mock.calls[0] as [number, number];
    expect(to - from).toBe(14 * 86400);
  });

  it('adds an event from the agenda with the times as UTC seconds and shows the list again', async () => {
    const createEvent = vi.fn().mockResolvedValue(undefined);
    const events = vi.fn().mockResolvedValue({ items: [], truncated: false });
    const a = api({ me: vi.fn().mockResolvedValue({ address: 'alice@one.test', csrf: 'tok' }), events, createEvent });
    render(() => <App api={a} />);
    fireEvent.click(await screen.findByText(en['folder.agenda']));
    fireEvent.input(await screen.findByLabelText(en['agenda.what']), { target: { value: 'Lunch' } });
    fireEvent.input(screen.getByLabelText(en['agenda.where']), { target: { value: 'Cafe' } });
    fireEvent.input(screen.getByLabelText(en['agenda.date']), { target: { value: '2030-05-06' } });
    fireEvent.input(screen.getByLabelText(en['agenda.from']), { target: { value: '12:00' } });
    fireEvent.input(screen.getByLabelText(en['agenda.until']), { target: { value: '13:30' } });
    fireEvent.submit(screen.getByLabelText(en['agenda.what']).closest('form') as HTMLFormElement);
    await vi.waitFor(() => expect(createEvent).toHaveBeenCalledTimes(1));
    const start = Math.floor(new Date('2030-05-06T12:00').getTime() / 1000);
    expect(createEvent).toHaveBeenCalledWith({ summary: 'Lunch', location: 'Cafe', start, end: start + 5400, all_day: false });
    await vi.waitFor(() => expect(events).toHaveBeenCalledTimes(2));
  });

  it('adds a whole-day event on the UTC midnights around its date', async () => {
    const createEvent = vi.fn().mockResolvedValue(undefined);
    const a = api({ me: vi.fn().mockResolvedValue({ address: 'alice@one.test', csrf: 'tok' }), createEvent });
    render(() => <App api={a} />);
    fireEvent.click(await screen.findByText(en['folder.agenda']));
    fireEvent.input(await screen.findByLabelText(en['agenda.what']), { target: { value: 'Holiday' } });
    fireEvent.input(screen.getByLabelText(en['agenda.date']), { target: { value: '2030-05-06' } });
    fireEvent.click(screen.getByLabelText(en['agenda.wholeDay']));
    fireEvent.submit(screen.getByLabelText(en['agenda.what']).closest('form') as HTMLFormElement);
    await vi.waitFor(() => expect(createEvent).toHaveBeenCalledTimes(1));
    const midnight = Date.UTC(2030, 4, 6) / 1000;
    expect(createEvent).toHaveBeenCalledWith({ summary: 'Holiday', location: '', start: midnight, end: midnight + 86400, all_day: true });
  });

  it('offers the contacts as a .vcf download from the settings', async () => {
    const a = api({ me: vi.fn().mockResolvedValue({ address: 'alice@one.test', csrf: 'tok' }) });
    render(() => <App api={a} />);
    fireEvent.click(await screen.findByText(en['folder.settings']));
    const link = await screen.findByText(en['settings.contacts.export']);
    expect(link.getAttribute('href')).toBe('/api/v1/contacts/export');
    expect(link.getAttribute('download')).toBe('contacts.vcf');
  });

  it('makes an app password after confirming the password and shows the secret once', async () => {
    const a = api({ me: vi.fn().mockResolvedValue({ address: 'alice@one.test', csrf: 'tok' }) });
    render(() => <App api={a} />);
    fireEvent.click(await screen.findByText(en['folder.settings']));
    fireEvent.input(await screen.findByLabelText(en['settings.apppw.label']), { target: { value: 'Phone' } });
    fireEvent.input(screen.getByLabelText(en['settings.apppw.confirm']), { target: { value: 'my-password-1' } });
    fireEvent.submit(screen.getByLabelText(en['settings.apppw.label']).closest('form') as HTMLFormElement);
    expect(await screen.findByText('abcd-efgh-jkmn-pqrs')).toBeTruthy();
    expect(a.createAppPassword).toHaveBeenCalledWith('Phone', 'my-password-1');
    fireEvent.click(screen.getByText(en['settings.apppw.done']));
    await waitFor(() => expect(screen.queryByText('abcd-efgh-jkmn-pqrs')).toBeNull());
  });

  it('saves a draft, and sends from it with the draft named so that it is removed', async () => {
    const a = api({ me: vi.fn().mockResolvedValue({ address: 'alice@one.test', csrf: 'tok' }) });
    render(() => <App api={a} />);
    fireEvent.click(await screen.findByText(en['compose.new']));
    fireEvent.input(await screen.findByLabelText(en['compose.to']), { target: { value: 'bob@one.test' } });
    fireEvent.click(screen.getByText(en['compose.save']));
    expect((await screen.findByText(en['compose.saved']))).toBeTruthy();
    expect(a.saveDraft).toHaveBeenCalledWith(expect.objectContaining({ to: ['bob@one.test'] }), undefined);
    fireEvent.click(screen.getByText(en['compose.send']));
    await waitFor(() => expect(a.send).toHaveBeenCalledWith(expect.objectContaining({ draft_id: 77 })));
  });

  it('starts a new message with the signature, and saves a changed one', async () => {
    const a = api({ me: vi.fn().mockResolvedValue({ address: 'alice@one.test', csrf: 'tok' }) });
    render(() => <App api={a} />);
    fireEvent.click(await screen.findByText(en['compose.new']));
    expect(((await screen.findByLabelText(en['compose.text'])) as HTMLTextAreaElement).value).toBe('\n\n-- \nAlice\n');
    fireEvent.click(screen.getByText(en['compose.cancel']));
    fireEvent.click(screen.getByText(en['folder.settings']));
    const box = (await screen.findByLabelText(en['settings.signature.label'])) as HTMLTextAreaElement;
    await waitFor(() => expect(box.value).toBe('Alice'));
    fireEvent.input(box, { target: { value: 'Alice B.' } });
    fireEvent.submit(box.closest('form') as HTMLFormElement);
    await waitFor(() => expect(a.saveSettings).toHaveBeenCalledWith('Alice B.'));
  });

  it('asks for the code after the password when the account has two-step sign-in, then signs in', async () => {
    const err = new ApiError(401, 'second_factor');
    const signIn = vi.fn().mockRejectedValueOnce(err).mockResolvedValueOnce({ address: 'alice@one.test', csrf: 'tok' });
    const a = api({ signIn });
    render(() => <App api={a} />);
    const address = await screen.findByLabelText(en['signin.address']);
    fireEvent.input(address, { target: { value: 'alice@one.test' } });
    fireEvent.input(screen.getByLabelText(en['signin.password']), { target: { value: 'pw-1' } });
    fireEvent.submit(address.closest('form') as HTMLFormElement);
    const code = await screen.findByLabelText(en['signin.code']);
    expect(screen.queryByRole('alert')).toBeNull();
    fireEvent.input(code, { target: { value: '123456' } });
    fireEvent.submit(code.closest('form') as HTMLFormElement);
    await waitFor(() => expect(signIn).toHaveBeenLastCalledWith('alice@one.test', 'pw-1', '123456'));
    expect(await screen.findByText(en['compose.new'])).toBeTruthy();
  });

  it('saves a filter, shows the line when the server refuses it, and turns one on', async () => {
    const refused = new ApiError(400, 'script_invalid', { line: 3, message: 'a block is not closed' });
    const a = api({
      me: vi.fn().mockResolvedValue({ address: 'alice@one.test', csrf: 'tok' }),
      saveScript: vi.fn().mockRejectedValueOnce(refused).mockResolvedValueOnce(undefined),
      scripts: vi.fn().mockResolvedValue({ scripts: [{ name: 'main', active: false, size: 5 }], max_scripts: 10, max_bytes: 65536 }),
    });
    render(() => <App api={a} />);
    fireEvent.click(await screen.findByText(en['folder.settings']));
    const box = (await screen.findByLabelText(en['filters.script'])) as HTMLTextAreaElement;
    fireEvent.input(box, { target: { value: 'if true {' } });
    fireEvent.submit(box.closest('form') as HTMLFormElement);
    expect((await screen.findByRole('alert')).textContent).toBe('The script is not correct. Line 3: a block is not closed');
    fireEvent.input(box, { target: { value: 'keep;' } });
    fireEvent.submit(box.closest('form') as HTMLFormElement);
    await waitFor(() => expect(a.saveScript).toHaveBeenLastCalledWith('main', 'keep;'));
    fireEvent.click(await screen.findByText(en['filters.turnOn']));
    await waitFor(() => expect(a.activateScript).toHaveBeenCalledWith('main'));
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
