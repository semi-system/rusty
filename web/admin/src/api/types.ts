// Generated from crates/rusty-admin/contract/v1.json by scripts/gen-types.mjs. Do not edit: change the contract and regenerate.

export type Empty = Record<string, never>;

export interface ErrorBody {
  error: string;
}

export interface Me {
  address: string;
  scope: string;
  domain: number | null;
  csrf: string;
}

export interface SetupState {
  required: boolean;
}

export interface Domain {
  id: number;
  name: string;
  status: string;
}

export interface Account {
  id: number;
  address: string;
  status: string;
  has_password: boolean;
}

export interface AccountCreated {
  id: number;
  address: string;
  status: string;
  has_password: boolean;
  aliases_created: string[];
}

export interface Alias {
  id: number;
  alias: string;
  target: string;
}

export interface AliasCreated {
  alias: string;
  target: string;
}

export interface DomainList {
  items: Domain[];
  next: number | null;
}

export interface AccountList {
  items: Account[];
  next: number | null;
}

export interface AliasList {
  items: Alias[];
  next: number | null;
}

export interface Dkim {
  selector: string;
  algorithm: string;
  record_name: string;
  record_value: string;
  sealed: boolean;
}

export interface DkimList {
  items: Dkim[];
}

export interface Relay {
  host: string;
  port: number;
  tls: string;
  username: string | null;
  spf_include: string | null;
  password_set: boolean;
  check_status: string | null;
  checked_at: number | null;
}

export interface RelayState {
  relay: Relay | null;
}

export interface Queued {
  queued: boolean;
}

export interface DnsRecord {
  name: string;
  type: string;
  value: string;
  ttl: number;
  purpose: string;
  required: string;
  status: string;
  seen: string | null;
  last_checked_at: number | null;
}

export interface DnsRecords {
  records: DnsRecord[];
  warnings: string[];
}

export interface Queue {
  queued: number;
  running: number;
  failed: number;
  dead: number;
}
