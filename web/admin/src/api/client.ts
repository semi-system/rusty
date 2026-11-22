// The API client: the contract's endpoints as typed calls. It knows the three rules of a state-changing request (design section 5): the JSON content type,
// the CSRF token of the session (the browser adds `Origin` itself) and that a failure is a fixed code, never text to show.

import type { Account, AccountCreated, AdminEntry, Alias, Dkim, DkimList, DnsRecords, Domain, Me, DomainSettings, ImportResult, MailList, Member, Members, Monitor, Plan, PlanList, QueueList, Routing, Trace, Protocols, Queue, Relay, SetupState, Storage } from './types';
import type { MessageKey } from '../i18n';

/** A page of a list, the cursor of the next one and how many rows match in all. */
export interface Page<T> {
  items: T[];
  next: number | null;
  total: number;
}

/** What narrows a list and sizes its page (the server checks every part of it). */
export interface ListOptions {
  /** A substring of the name. */
  q?: string;
  /** One status of the list's kind. */
  status?: string;
  /** Rows per page, 1 to 100. */
  limit?: number;
}

/** What `PUT .../relay` takes (the password only together with a user name). */
export interface RelayInput {
  host: string;
  port: number;
  tls: 'starttls' | 'implicit' | 'none';
  username?: string;
  spf_include?: string;
  password?: string;
}

/** What `PUT .../storage` takes: the S3 settings and both keys (sealed by the server, never returned). */
export interface StorageInput {
  endpoint: string;
  region: string;
  bucket: string;
  prefix: string;
  path_style: boolean;
  access_key: string;
  secret_key: string;
}

/** What `PUT .../storage/pool` takes. */
export interface PoolInput {
  copies: number;
  sync_copies: number;
  placement: 'ordered' | 'weighted';
  read: 'located' | 'ordered' | 'round_robin' | 'fastest';
}

/** What `POST .../storage/backends` takes. */
export interface BackendInput extends StorageInput {
  name: string;
  weight: number;
}

const seg = encodeURIComponent;
const cursor = (after?: number, opts: ListOptions = {}): string => {
  const parts: string[] = [];
  if (after !== undefined) parts.push(`after=${after}`);
  if (opts.q !== undefined && opts.q.trim() !== '') parts.push(`q=${encodeURIComponent(opts.q.trim())}`);
  if (opts.status !== undefined && opts.status !== '') parts.push(`status=${encodeURIComponent(opts.status)}`);
  // 100 is the server's own page size: nothing to say.
  if (opts.limit !== undefined && opts.limit !== 100) parts.push(`limit=${opts.limit}`);
  return parts.length === 0 ? '' : `?${parts.join('&')}`;
};

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

const KNOWN = new Set(['unauthorized', 'invalid', 'forbidden', 'not_found', 'exists', 'not_closed', 'not_empty', 'wrong_state', 'limit_reached', 'unsupported', 'throttled', 'busy', 'unavailable']);

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

  domains(after?: number, opts?: ListOptions): Promise<Page<Domain>> {
    return this.call('GET', `/api/v1/domains${cursor(after, opts)}`);
  }

  createDomain(name: string): Promise<Domain> {
    return this.call('POST', '/api/v1/domains', { name });
  }

  setDomainEnabled(name: string, enabled: boolean): Promise<Domain> {
    return this.call('POST', `/api/v1/domains/${seg(name)}/${enabled ? 'enable' : 'disable'}`);
  }

  accounts(domain: string, after?: number, opts?: ListOptions): Promise<Page<Account>> {
    return this.call('GET', `/api/v1/domains/${seg(domain)}/accounts${cursor(after, opts)}`);
  }

  createAccount(domain: string, local: string, password?: string): Promise<AccountCreated> {
    return this.call('POST', `/api/v1/domains/${seg(domain)}/accounts`, password === undefined || password === '' ? { local } : { local, password });
  }

  async setPassword(address: string, password: string, mustChange = false): Promise<void> {
    await this.call('POST', `/api/v1/accounts/${seg(address)}/password`, { password, must_change: mustChange });
  }

  setAccountStatus(address: string, action: 'enable' | 'lock' | 'disable'): Promise<Account> {
    return this.call('POST', `/api/v1/accounts/${seg(address)}/${action}`);
  }

  /** Make accounts from CSV text (at most 500 lines a request); with `dryRun` nothing is changed and the answer says what would happen. */
  importAccounts(domain: string, csv: string, dryRun: boolean): Promise<ImportResult> {
    return this.call('POST', `/api/v1/domains/${seg(domain)}/accounts/import`, { csv, dry_run: dryRun });
  }

  /** The defaults, limits and password rules of a domain. */
  domainSettings(domain: string): Promise<DomainSettings> {
    return this.call('GET', `/api/v1/domains/${seg(domain)}/settings`);
  }

  setDomainSettings(domain: string, settings: DomainSettings): Promise<DomainSettings> {
    return this.call('PUT', `/api/v1/domains/${seg(domain)}/settings`, settings);
  }

  /** The outbound queue of a domain, newest first. */
  queue(domain: string, opts: { state?: string; q?: string; before?: number } = {}): Promise<QueueList> {
    const parts: string[] = [];
    if (opts.state !== undefined && opts.state !== '') parts.push(`state=${encodeURIComponent(opts.state)}`);
    if (opts.q !== undefined && opts.q.trim() !== '') parts.push(`q=${encodeURIComponent(opts.q.trim())}`);
    if (opts.before !== undefined) parts.push(`before=${opts.before}`);
    return this.call('GET', `/api/v1/domains/${seg(domain)}/queue${parts.length > 0 ? '?' + parts.join('&') : ''}`);
  }

  async queueAction(domain: string, id: number, action: 'hold' | 'release' | 'retry' | 'delete'): Promise<void> {
    const path = `/api/v1/domains/${seg(domain)}/queue/${id}`;
    if (action === 'delete') await this.call('DELETE', path);
    else await this.call('POST', `${path}/${action}`);
  }

  /** The delivery log of a domain's outgoing mail, newest first. */
  trace(domain: string, opts: { q?: string; before?: number } = {}): Promise<Trace> {
    const parts: string[] = [];
    if (opts.q !== undefined && opts.q.trim() !== '') parts.push(`q=${encodeURIComponent(opts.q.trim())}`);
    if (opts.before !== undefined) parts.push(`before=${opts.before}`);
    return this.call('GET', `/api/v1/domains/${seg(domain)}/trace${parts.length > 0 ? '?' + parts.join('&') : ''}`);
  }

  /** Lists, the catch-all and the alias domains of a domain. */
  routing(domain: string): Promise<Routing> {
    return this.call('GET', `/api/v1/domains/${seg(domain)}/routing`);
  }

  createList(domain: string, local: string): Promise<MailList> {
    return this.call('POST', `/api/v1/domains/${seg(domain)}/lists`, { local });
  }

  async deleteList(domain: string, local: string): Promise<void> {
    await this.call('DELETE', `/api/v1/domains/${seg(domain)}/lists/${seg(local)}`);
  }

  async listMembers(domain: string, local: string): Promise<Member[]> {
    return (await this.call<Members>('GET', `/api/v1/domains/${seg(domain)}/lists/${seg(local)}/members`)).items;
  }

  async addListMember(domain: string, local: string, address: string): Promise<Member[]> {
    return (await this.call<Members>('POST', `/api/v1/domains/${seg(domain)}/lists/${seg(local)}/members`, { address })).items;
  }

  async removeListMember(domain: string, local: string, seq: number): Promise<void> {
    await this.call('DELETE', `/api/v1/domains/${seg(domain)}/lists/${seg(local)}/members/${seq}`);
  }

  async setCatchAll(domain: string, local: string | null): Promise<void> {
    await this.call('PUT', `/api/v1/domains/${seg(domain)}/catch-all`, { local });
  }

  async addDomainAlias(domain: string, name: string): Promise<void> {
    await this.call('POST', `/api/v1/domains/${seg(domain)}/domain-aliases`, { name });
  }

  async removeDomainAlias(domain: string, name: string): Promise<void> {
    await this.call('DELETE', `/api/v1/domains/${seg(domain)}/domain-aliases/${seg(name)}`);
  }

  /** Delete a closed account and its mail: the server removes it at its next collection (a closed account goes by itself after the retention). */
  async deleteAccount(address: string): Promise<void> {
    await this.call('DELETE', `/api/v1/accounts/${seg(address)}`);
  }

  /** Delete a domain that holds nothing (platform administrators). */
  async deleteDomain(name: string): Promise<void> {
    await this.call('DELETE', `/api/v1/domains/${seg(name)}`);
  }

  aliases(domain: string, after?: number, opts?: ListOptions): Promise<Page<Alias>> {
    return this.call('GET', `/api/v1/domains/${seg(domain)}/aliases${cursor(after, opts)}`);
  }

  async createAlias(domain: string, local: string, target: string): Promise<void> {
    await this.call('POST', `/api/v1/domains/${seg(domain)}/aliases`, { local, target });
  }

  async deleteAlias(address: string): Promise<void> {
    await this.call('DELETE', `/api/v1/aliases/${seg(address)}`);
  }

  admins(after?: number, opts?: ListOptions): Promise<Page<AdminEntry>> {
    return this.call('GET', `/api/v1/admins${cursor(after, opts)}`);
  }

  grantAdmin(address: string, domain: string | null): Promise<AdminEntry> {
    return this.call('POST', '/api/v1/admins', { address, domain });
  }

  async revokeAdmin(address: string): Promise<void> {
    await this.call('DELETE', `/api/v1/admins/${seg(address)}`);
  }

  async dkim(domain: string): Promise<Dkim[]> {
    return (await this.call<DkimList>('GET', `/api/v1/domains/${seg(domain)}/dkim`)).items;
  }

  createDkim(domain: string, selector?: string): Promise<Dkim> {
    return this.call('POST', `/api/v1/domains/${seg(domain)}/dkim`, selector === undefined || selector === '' ? {} : { selector });
  }

  /** The relay of the domain, or `null` when it has none of its own. */
  async relay(domain: string): Promise<Relay | null> {
    return (await this.call<{ relay: Relay | null }>('GET', `/api/v1/domains/${seg(domain)}/relay`)).relay;
  }

  async putRelay(domain: string, input: RelayInput): Promise<Relay> {
    const r = await this.call<{ relay: Relay }>('PUT', `/api/v1/domains/${seg(domain)}/relay`, input);
    return r.relay;
  }

  async deleteRelay(domain: string): Promise<void> {
    await this.call('DELETE', `/api/v1/domains/${seg(domain)}/relay`);
  }

  async checkRelay(domain: string): Promise<void> {
    await this.call('POST', `/api/v1/domains/${seg(domain)}/relay/check`);
  }

  /** The store of the domain, or `null` when it uses the platform store. */
  async storage(domain: string): Promise<Storage | null> {
    return (await this.call<{ storage: Storage | null }>('GET', `/api/v1/domains/${seg(domain)}/storage`)).storage;
  }

  async putStorage(domain: string, input: StorageInput): Promise<Storage | null> {
    return (await this.call<{ storage: Storage | null }>('PUT', `/api/v1/domains/${seg(domain)}/storage`, input)).storage;
  }

  async putPool(domain: string, input: PoolInput): Promise<Storage | null> {
    return (await this.call<{ storage: Storage | null }>('PUT', `/api/v1/domains/${seg(domain)}/storage/pool`, input)).storage;
  }

  async addBackend(domain: string, input: BackendInput): Promise<Storage | null> {
    return (await this.call<{ storage: Storage | null }>('POST', `/api/v1/domains/${seg(domain)}/storage/backends`, input)).storage;
  }

  async putBackend(domain: string, backend: string, input: { state: string; weight: number }): Promise<Storage | null> {
    return (await this.call<{ storage: Storage | null }>('PUT', `/api/v1/domains/${seg(domain)}/storage/backends/${seg(backend)}`, input)).storage;
  }

  async backendKeys(domain: string, backend: string, input: { access_key: string; secret_key: string }): Promise<Storage | null> {
    return (await this.call<{ storage: Storage | null }>('POST', `/api/v1/domains/${seg(domain)}/storage/backends/${seg(backend)}/keys`, input)).storage;
  }

  async deleteBackend(domain: string, backend: string): Promise<void> {
    await this.call('DELETE', `/api/v1/domains/${seg(domain)}/storage/backends/${seg(backend)}`);
  }

  async checkStorage(domain: string): Promise<void> {
    await this.call('POST', `/api/v1/domains/${seg(domain)}/storage/check`);
  }

  dns(domain: string): Promise<DnsRecords> {
    return this.call('GET', `/api/v1/domains/${seg(domain)}/dns`);
  }

  async checkDns(domain: string): Promise<void> {
    await this.call('POST', `/api/v1/domains/${seg(domain)}/dns/check`);
  }

  outbound(domain: string): Promise<Queue> {
    return this.call('GET', `/api/v1/domains/${seg(domain)}/outbound`);
  }

  /** The services, the outbound queue, the storage checks and the DNS problems (platform administrators only). */
  monitor(): Promise<Monitor> {
    return this.call('GET', '/api/v1/monitor');
  }

  /** The optional protocols and whether each is on (platform administrators only). */
  protocols(): Promise<Protocols> {
    return this.call('GET', '/api/v1/protocols');
  }

  /** The plans (any administrator). */
  async plans(): Promise<Plan[]> {
    return (await this.call<PlanList>('GET', '/api/v1/plans')).items;
  }

  /** Create a plan or change its limits (platform administrators): a null limit removes it. */
  putPlan(name: string, quotaBytes: number | null, maxMessageBytes: number | null, messagesPerHour: number | null = null, protocols: string[] | null = null): Promise<Plan> {
    return this.call('PUT', '/api/v1/plans', { name, quota_bytes: quotaBytes, max_message_bytes: maxMessageBytes, messages_per_hour: messagesPerHour, protocols });
  }

  /** Put an account on a plan, or on none. */
  async setAccountPlan(address: string, plan: string | null): Promise<void> {
    await this.call('PUT', `/api/v1/accounts/${seg(address)}/plan`, { plan });
  }

  /** Switch a protocol on or off for every replica of the mail services. */
  setProtocol(protocol: string, enabled: boolean): Promise<Protocols> {
    return this.call('PUT', '/api/v1/protocols', { protocol, enabled });
  }
}
