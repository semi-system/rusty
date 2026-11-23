/** The client of the webmail API (docs/design-webmail.md). The CSRF token lives in memory only; the session is the `__Host-` cookie the browser keeps. */

export class ApiError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
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
  subject: string;
  message_id: string;
  text: string;
  text_truncated: boolean;
  html_only: boolean;
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
}
export type Action = 'read' | 'unread' | 'flag' | 'unflag' | 'move' | 'copy' | 'delete';

export interface Api {
  me(): Promise<Me>;
  signIn(address: string, password: string): Promise<Me>;
  signOut(): Promise<void>;
  folders(): Promise<Folder[]>;
  messages(folder: number, before?: number): Promise<Page>;
  message(id: number): Promise<Message>;
  actions(ids: number[], action: Action, folder?: number): Promise<{ applied: number; missing: number[] }>;
  emptyFolder(folder: number): Promise<void>;
  send(draft: Draft): Promise<{ id: number }>;
  outbox(): Promise<OutboxItem[]>;
  withdraw(id: number): Promise<void>;
  rawUrl(id: number): string;
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
      throw new ApiError(res.status, code);
    }
    return data as T;
  }
  const remember = (m: Me): Me => {
    csrf = m.csrf;
    return m;
  };
  return {
    me: async () => remember(await call<Me>('GET', '/api/v1/me')),
    signIn: async (address, password) => remember(await call<Me>('POST', '/api/v1/session', { address, password })),
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
    outbox: async () => (await call<{ items: OutboxItem[] }>('GET', '/api/v1/outbox')).items,
    withdraw: async (id) => {
      await call('DELETE', `/api/v1/outbox/${id}`);
    },
    rawUrl: (id) => `/api/v1/messages/${id}/raw`,
  };
}

/** The addresses of a field: split at commas, semicolons and white space; empty parts dropped. */
export function addressesOf(field: string): string[] {
  return field.split(/[\s,;]+/).filter((a) => a !== '');
}
