/** The client of the webmail API (docs/design-webmail.md). The CSRF token lives in memory only; the session is the `__Host-` cookie the browser keeps. */

export class ApiError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    /** What else the server said (a refused script: the line and the words of the parser). */
    readonly detail: Record<string, unknown> = {},
  ) {
    super(code);
  }
}

export interface Me {
  address: string;
  csrf: string;
}
export interface Folder {
  id: number;
  name: string;
  role: string;
  parent: number | null;
  messages: number;
  unseen: number;
}
export interface Flags {
  seen: boolean;
  flagged: boolean;
  answered: boolean;
  draft: boolean;
}
export interface Item {
  id: number;
  uid: number;
  subject: string;
  from: string;
  date: number;
  size: number;
  flags: Flags;
}
export interface Page {
  items: Item[];
  next: number | null;
}
export interface Attachment {
  name: string | null;
  content_type: string;
  size: number;
}
export interface Message {
  id: number;
  folder: number;
  date: number;
  size: number;
  flags: Flags;
  from: string;
  to: string;
  cc: string;
  bcc: string;
  subject: string;
  message_id: string;
  text: string;
  text_truncated: boolean;
  html_only: boolean;
  has_html: boolean;
  attachments: Attachment[];
}
export interface OutboxItem {
  id: number;
  subject: string;
  recipients: string[];
  state: 'queued' | 'sending' | 'sent' | 'failed';
  error: string | null;
  created: number;
  finished: number | null;
}
export interface Draft {
  to: string[];
  cc: string[];
  bcc: string[];
  subject: string;
  text: string;
  in_reply_to?: string;
  /// The draft this was made from; it is removed when the message is queued.
  draft_id?: number;
}
export interface AppPassword {
  id: number;
  label: string;
  created: number;
  last_used: number | null;
}
export interface TotpStatus {
  available: boolean;
  enabled: boolean;
  pending: boolean;
}
export interface ScriptInfo {
  name: string;
  active: boolean;
  size: number;
}
export interface Scripts {
  scripts: ScriptInfo[];
  max_scripts: number;
  max_bytes: number;
}
export type Action = 'read' | 'unread' | 'flag' | 'unflag' | 'move' | 'copy' | 'delete';

/** A suggestion for an address field: a name and one address. */
export interface Contact {
  name: string;
  email: string;
}

/** What an import of contacts did, summed over its requests. */
export interface ImportResult {
  imported: number;
  updated: number;
  invalid: number;
  duplicates: number;
}

/** One event of the agenda: a UTC time is shown in the reader's zone, any other as the wall clock it states. */
export interface AgendaItem {
  uid: string;
  summary: string;
  location: string;
  start: number;
  end: number;
  all_day: boolean;
  utc: boolean;
  tzid: string | null;
  unexpanded: boolean;
  calendar: string;
}

export interface AgendaPage {
  items: AgendaItem[];
  truncated: boolean;
}

export interface Api {
  me(): Promise<Me>;
  signIn(address: string, password: string, code?: string): Promise<Me>;
  signOut(): Promise<void>;
  folders(): Promise<Folder[]>;
  messages(folder: number, before?: number): Promise<Page>;
  message(id: number): Promise<Message>;
  actions(ids: number[], action: Action, folder?: number): Promise<{ applied: number; missing: number[] }>;
  emptyFolder(folder: number): Promise<void>;
  send(draft: Draft): Promise<{ id: number }>;
  saveDraft(draft: Draft, replaces?: number): Promise<{ id: number }>;
  outbox(): Promise<OutboxItem[]>;
  withdraw(id: number): Promise<void>;
  rawUrl(id: number): string;
  htmlUrl(id: number): string;
  attachmentUrl(id: number, n: number): string;
  appPasswords(): Promise<AppPassword[]>;
  createAppPassword(label: string, password: string): Promise<{ id: number; label: string; secret: string }>;
  revokeAppPassword(id: number): Promise<void>;
  scripts(): Promise<Scripts>;
  script(name: string): Promise<{ name: string; body: string }>;
  saveScript(name: string, body: string): Promise<void>;
  activateScript(name: string | null): Promise<void>;
  deleteScript(name: string): Promise<void>;
  totpStatus(): Promise<TotpStatus>;
  totpBegin(password: string): Promise<{ secret: string; uri: string }>;
  totpConfirm(code: string): Promise<{ recovery: string[] }>;
  totpDisable(password: string, code: string): Promise<void>;
  settings(): Promise<{ signature: string }>;
  saveSettings(signature: string): Promise<{ signature: string }>;
  /** The contacts of the person whose name or address holds `q` (two characters or more): what the composer suggests. */
  contacts(q: string): Promise<Contact[]>;
  /** The events of the person that meet `[from, to)` (unix seconds), the repeating ones expanded. */
  events(from: number, to: number): Promise<AgendaPage>;
  /** Adds the cards of the text of a .vcf file to the address book. */
  importContacts(vcf: string): Promise<ImportResult>;
}

type Fetch = (input: string, init?: RequestInit) => Promise<Response>;

/** The API over `fetch`. A state-changing call carries the token of the session and says it is JSON; any failure is an `ApiError` with the fixed code of the server. */
export function createApi(doFetch: Fetch = (i, init) => fetch(i, init)): Api {
  let csrf = '';
  async function call<T>(method: string, path: string, body?: unknown): Promise<T> {
    const headers: Record<string, string> = {};
    const init: RequestInit = { method, headers, credentials: 'same-origin' };
    if (method !== 'GET') {
      headers['content-type'] = 'application/json';
      headers['x-csrf-token'] = csrf;
      if (method !== 'DELETE') {
        init.body = JSON.stringify(body ?? {});
      }
    }
    let res: Response;
    try {
      res = await doFetch(path, init);
    } catch {
      throw new ApiError(0, 'network');
    }
    let data: unknown;
    try {
      data = await res.json();
    } catch {
      data = null;
    }
    if (!res.ok) {
      const code = typeof data === 'object' && data !== null && 'error' in data ? String((data as { error: unknown }).error) : 'unknown';
      throw new ApiError(res.status, code, typeof data === 'object' && data !== null ? (data as Record<string, unknown>) : {});
    }
    return data as T;
  }
  const remember = (m: Me): Me => {
    csrf = m.csrf;
    return m;
  };
  return {
    me: async () => remember(await call<Me>('GET', '/api/v1/me')),
    signIn: async (address, password, code) => remember(await call<Me>('POST', '/api/v1/session', code === undefined ? { address, password } : { address, password, code })),
    signOut: async () => {
      await call('DELETE', '/api/v1/session');
      csrf = '';
    },
    folders: async () => (await call<{ items: Folder[] }>('GET', '/api/v1/folders')).items,
    messages: (folder, before) => call<Page>('GET', `/api/v1/folders/${folder}/messages?limit=50${before === undefined ? '' : `&before=${before}`}`),
    message: (id) => call<Message>('GET', `/api/v1/messages/${id}`),
    actions: (ids, action, folder) => call('POST', '/api/v1/messages/actions', folder === undefined ? { ids, action } : { ids, action, folder }),
    emptyFolder: async (folder) => {
      await call('POST', `/api/v1/folders/${folder}/empty`);
    },
    send: (draft) => call('POST', '/api/v1/send', draft),
    saveDraft: (draft, replaces) => call('POST', '/api/v1/drafts', replaces === undefined ? draft : { ...draft, id: replaces }),
    outbox: async () => (await call<{ items: OutboxItem[] }>('GET', '/api/v1/outbox')).items,
    withdraw: async (id) => {
      await call('DELETE', `/api/v1/outbox/${id}`);
    },
    rawUrl: (id) => `/api/v1/messages/${id}/raw`,
    htmlUrl: (id) => `/api/v1/messages/${id}/html`,
    attachmentUrl: (id, n) => `/api/v1/messages/${id}/attachments/${n}`,
    appPasswords: async () => (await call<{ items: AppPassword[] }>('GET', '/api/v1/app-passwords')).items,
    createAppPassword: (label, password) => call('POST', '/api/v1/app-passwords', { label, password }),
    revokeAppPassword: async (id) => {
      await call('DELETE', `/api/v1/app-passwords/${id}`);
    },
    scripts: () => call('GET', '/api/v1/sieve'),
    script: (name) => call('GET', `/api/v1/sieve/${encodeURIComponent(name)}`),
    saveScript: async (name, body) => {
      await call('PUT', `/api/v1/sieve/${encodeURIComponent(name)}`, { body });
    },
    activateScript: async (name) => {
      await call('POST', name === null ? '/api/v1/sieve/deactivate' : `/api/v1/sieve/${encodeURIComponent(name)}/activate`);
    },
    deleteScript: async (name) => {
      await call('DELETE', `/api/v1/sieve/${encodeURIComponent(name)}`);
    },
    totpStatus: () => call('GET', '/api/v1/totp'),
    totpBegin: (password) => call('POST', '/api/v1/totp/begin', { password }),
    totpConfirm: (code) => call('POST', '/api/v1/totp/confirm', { code }),
    totpDisable: async (password, code) => {
      await call('POST', '/api/v1/totp/disable', { password, code });
    },
    settings: () => call('GET', '/api/v1/settings'),
    contacts: async (q) => (await call<{ items: Contact[] }>('GET', `/api/v1/contacts?q=${encodeURIComponent(q)}`)).items,
    events: (from, to) => call<AgendaPage>('GET', `/api/v1/events?from=${from}&to=${to}`),
    importContacts: (vcf) => call<ImportResult>('POST', '/api/v1/contacts/import', { vcf }),
    saveSettings: (signature) => call('PUT', '/api/v1/settings', { signature }),
  };
}

/** The addresses of a field: split at commas, semicolons and white space; empty parts dropped. */
export function addressesOf(field: string): string[] {
  return field.split(/[\s,;]+/).filter((a) => a !== '');
}
