// The API client: the contract's endpoints as typed calls. It knows the three rules of a state-changing request (design section 5): the JSON content type,
// the CSRF token of the session (the browser adds `Origin` itself) and that a failure is a fixed code, never text to show.

import type { Domain, Me, SetupState } from './types';
import type { MessageKey } from '../i18n';

/** A failed call: the status, the fixed error code of the contract, and the delay of a throttled one. */
export class ApiError extends Error {
  readonly status: number;
  readonly code: string;
  readonly retryAfter: number | null;

  constructor(status: number, code: string, retryAfter: number | null = null) {
    super(code);
    this.name = 'ApiError';
    this.status = status;
    this.code = code;
    this.retryAfter = retryAfter;
  }
}

const KNOWN = new Set(['unauthorized', 'invalid', 'forbidden', 'not_found', 'exists', 'unsupported', 'throttled', 'busy', 'unavailable']);

/** The catalogue key of an error and its parameters. The server never sends text for people; the wording is ours. */
export function errorMessage(e: unknown): { key: MessageKey; params: Record<string, number> } {
  if (e instanceof ApiError) {
    if (e.code === 'throttled') {
      return e.retryAfter !== null ? { key: 'error.throttled', params: { seconds: e.retryAfter } } : { key: 'error.throttledNow', params: {} };
    }
    if (KNOWN.has(e.code)) {
      return { key: `error.${e.code}` as MessageKey, params: {} };
    }
    return { key: 'error.unknown', params: {} };
  }
  return { key: 'error.network', params: {} };
}

export type Fetch = (input: string, init?: RequestInit) => Promise<Response>;

export class Client {
  private csrf: string | null = null;
  private readonly fetcher: Fetch;

  constructor(fetcher: Fetch = (input, init) => fetch(input, init)) {
    this.fetcher = fetcher;
  }

  /** The CSRF token of the current session, once known. */
  get csrfToken(): string | null {
    return this.csrf;
  }

  private async call<T>(method: string, path: string, body?: unknown): Promise<T> {
    const headers: Record<string, string> = { Accept: 'application/json' };
    const mutating = method !== 'GET';
    if (mutating) {
      headers['Content-Type'] = 'application/json';
      if (this.csrf !== null) {
        headers['X-CSRF-Token'] = this.csrf;
      }
    }
    let res: Response;
    try {
      res = await this.fetcher(path, {
        method,
        headers,
        credentials: 'same-origin',
        cache: 'no-store',
        body: body === undefined ? (mutating ? '{}' : undefined) : JSON.stringify(body),
      });
    } catch {
      throw new TypeError('network');
    }
    let data: unknown;
    try {
      data = await res.json();
    } catch {
      data = null;
    }
    if (!res.ok) {
      const code = typeof data === 'object' && data !== null && 'error' in data && typeof (data as { error: unknown }).error === 'string' ? (data as { error: string }).error : 'unknown';
      const ra = res.headers.get('retry-after');
      const retry = ra !== null && /^\d+$/.test(ra) ? Number(ra) : null;
      throw new ApiError(res.status, code, retry);
    }
    return data as T;
  }

  /** Who is signed in; `null` when nobody. Also remembers the CSRF token. */
  async me(): Promise<Me | null> {
    try {
      const me = await this.call<Me>('GET', '/api/v1/session');
      this.csrf = me.csrf;
      return me;
    } catch (e) {
      if (e instanceof ApiError && e.status === 401) {
        this.csrf = null;
        return null;
      }
      throw e;
    }
  }

  async signIn(address: string, password: string): Promise<Me> {
    const me = await this.call<Me>('POST', '/api/v1/session', { address, password });
    this.csrf = me.csrf;
    return me;
  }

  async signOut(): Promise<void> {
    await this.call('DELETE', '/api/v1/session');
    this.csrf = null;
  }

  /** Whether the first-administrator page is open (`false` once an administrator exists: the server answers 404). */
  async setupOpen(): Promise<boolean> {
    try {
      const s = await this.call<SetupState>('GET', '/api/v1/setup');
      return s.required;
    } catch (e) {
      if (e instanceof ApiError && e.status === 404) {
        return false;
      }
      throw e;
    }
  }

  async setup(token: string, address: string, password: string): Promise<Me> {
    await this.call('POST', '/api/v1/setup', { token, address, password });
    // The server signs the new administrator in; ask who that is (this also gives the CSRF token).
    const me = await this.me();
    if (me === null) {
      throw new ApiError(401, 'unauthorized');
    }
    return me;
  }

  async domains(after?: number): Promise<{ items: Domain[]; next: number | null }> {
    return this.call('GET', after === undefined ? '/api/v1/domains' : `/api/v1/domains?after=${after}`);
  }
}
