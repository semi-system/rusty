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
  last_login_at: number | null;
  plan: string | null;
  used_bytes: number;
  quota_bytes: number | null;
  last_delivery_at: number | null;
  must_change_password: boolean;
}

export interface AccountCreated {
  id: number;
  address: string;
  status: string;
  has_password: boolean;
  last_login_at: number | null;
  plan: string | null;
  used_bytes: number;
  quota_bytes: number | null;
  last_delivery_at: number | null;
  must_change_password: boolean;
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
  total: number;
}

export interface AccountList {
  items: Account[];
  next: number | null;
  total: number;
}

export interface AliasList {
  items: Alias[];
  next: number | null;
  total: number;
}

export interface Hint {
  topics: string[];
}

export interface AdminEntry {
  id: number;
  address: string;
  scope: string;
  role: string;
  domain: string | null;
  created_at: number;
}

export interface AdminList {
  items: AdminEntry[];
  next: number | null;
  total: number;
}

export interface Dkim {
  selector: string;
  algorithm: string;
  record_name: string;
  record_value: string;
  sealed: boolean;
  created_at: number;
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

export interface StorageSettings {
  endpoint: string;
  region: string;
  bucket: string;
  prefix: string;
  path_style: boolean;
}

export interface StoragePolicy {
  copies: number;
  sync_copies: number;
  placement: string;
  read: string;
}

export interface StorageBackend {
  name: string;
  state: string;
  weight: number;
  check_status: string;
  checked_at: number | null;
  settings: StorageSettings | null;
  access_key_set: boolean | null;
  secret_key_set: boolean | null;
  objects: number | null;
}

export interface StorageCapacity {
  declared_bytes: number | null;
  used_bytes: number | null;
  promised_bytes: number | null;
  unlimited_accounts: number | null;
  status: string;
}

export interface Storage {
  capacity: StorageCapacity | null;
  kind: string;
  ready: boolean;
  check_status: string;
  checked_at: number | null;
  settings: StorageSettings | null;
  access_key_set: boolean | null;
  secret_key_set: boolean | null;
  policy: StoragePolicy | null;
  backends: StorageBackend[] | null;
}

export interface StorageState {
  storage: Storage | null;
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

export interface ExtraRecord {
  id: number;
  name: string;
  type: string;
  value: string;
  ttl: number;
}

export interface DnsHosting {
  mode: string;
  name_servers: string[];
  extra: ExtraRecord[];
  max_extra: number;
}

export interface Queue {
  queued: number;
  running: number;
  failed: number;
  dead: number;
}

export interface MonitorInstance {
  instance: string;
  version: string;
  uptime_secs: number;
  age_secs: number;
  stale: boolean;
  replaced: boolean;
  status: string;
  listeners: number | null;
}

export interface MonitorService {
  service: string;
  status: string;
  instances: MonitorInstance[];
}

export interface MonitorQueue {
  queued: number;
  running: number;
  failed: number;
  dead: number;
  oldest_due_secs: number;
}

export interface MonitorCount {
  status: string;
  count: number;
}

export interface Monitor {
  services: MonitorService[];
  queue: MonitorQueue;
  storage: MonitorCount[];
  dns_problem_domains: number;
}

export interface Protocol {
  name: string;
  enabled: boolean;
}

export interface Protocols {
  protocols: Protocol[];
}

export interface Plan {
  name: string;
  quota_bytes: number | null;
  max_message_bytes: number | null;
  messages_per_hour: number | null;
  protocols: string[] | null;
  spam_junk_score: number | null;
  spam_reject_score: number | null;
  spam_retention_days: number | null;
  retention_days: number | null;
}

export interface PlanList {
  items: Plan[];
}

export interface AccountPlan {
  plan: string | null;
}

export interface Deletion {
  deletion: string;
}

export interface MailList {
  local: string;
  address: string;
  members: number;
}

export interface Routing {
  lists: MailList[];
  catch_all: string | null;
  domain_aliases: string[];
}

export interface Member {
  seq: number;
  address: string;
  external: boolean;
}

export interface Members {
  items: Member[];
}

export interface CatchAll {
  catch_all: string | null;
}

export interface DomainAliasName {
  name: string;
}

export interface SearchFound {
  account: string;
  folder: string;
  at: number;
  from: string;
  subject: string;
  size: number;
}

export interface Search {
  id: number;
  state: string;
  found: number;
  truncated: boolean;
  results: SearchFound[];
  error: string | null;
  created_at: number;
  finished_at: number | null;
}

export interface SearchQueued {
  id: number;
  state: string;
}

export interface PlatformSettings {
  quota_bytes: number | null;
  max_message_bytes: number | null;
  messages_per_hour: number | null;
  virus_action: string | null;
  blocked_extensions: string[];
  admin_search: boolean;
}

export interface DomainSettings {
  blocked_extensions: string[];
  footer_text: string | null;
  sender_allow: string[];
  sender_block: string[];
  default_plan: string | null;
  max_accounts: number | null;
  quota_bytes: number | null;
  max_message_bytes: number | null;
  password_min_length: number | null;
  password_classes: number | null;
}

export interface ImportRow {
  line: number;
  address: string;
  result: string;
  reason: string | null;
}

export interface ImportResult {
  dry_run: boolean;
  created: number;
  exists: number;
  invalid: number;
  rows: ImportRow[];
}

export interface QueueJob {
  id: number;
  state: string;
  sender: string;
  recipients: string[];
  destination: string;
  attempts: number;
  created_at: number;
  next_attempt_at: number | null;
  last_error: string | null;
}

export interface QueueList {
  items: QueueJob[];
  next: number | null;
}

export interface LogLine {
  id: number;
  direction: string;
  at: number;
  sender: string;
  recipient: string;
  event: string;
  remote: string | null;
  status: string | null;
  detail: string | null;
}

export interface Trace {
  items: LogLine[];
  next: number | null;
}

export interface Transfer {
  id: number;
  kind: string;
  requested_by: string;
  state: string;
  folder: string | null;
  size: number;
  messages: number;
  skipped: number;
  error: string | null;
  created_at: number;
  finished_at: number | null;
  expires_at: number;
}

export interface TransferList {
  items: Transfer[];
}

export interface MailForward {
  to: string[];
  keep: boolean;
}

export interface MailVacation {
  enabled: boolean;
  subject: string;
  body: string;
  from: number | null;
  until: number | null;
}

export interface MailRules {
  forward: MailForward | null;
  vacation: MailVacation | null;
}

export interface Certificate {
  host: string;
  port: number;
  purpose: string;
  subject: string | null;
  issuer: string | null;
  not_before: number | null;
  not_after: number | null;
  days_left: number | null;
  error: string | null;
}

export interface CertificateList {
  items: Certificate[];
}

export interface DayStats {
  day: number;
  received: number;
  rejected: number;
  sent: number;
  deferred: number;
  failed: number;
}

export interface Stats {
  days: DayStats[];
}

export interface AuditEntry {
  id: number;
  at: number;
  actor: string;
  domain_id: number | null;
  ip: string;
  method: string;
  route: string;
  path: string;
  status: number;
  detail: string | null;
}

export interface BackupRun {
  id: number;
  at: number;
  kind: string;
  ok: boolean;
  bytes: number | null;
  detail: string;
}

export interface Backups {
  now: number;
  items: BackupRun[];
}

export interface Migration {
  id: number;
  host: string;
  port: number;
  security: string;
  user: string;
  requested_by: string;
  state: string;
  stopping: boolean;
  folder: string | null;
  folders_total: number;
  folders_done: number;
  messages: number;
  skipped: number;
  bytes: number;
  error: string | null;
  created_at: number;
  finished_at: number | null;
}

export interface MigrationList {
  items: Migration[];
}

export interface Audit {
  items: AuditEntry[];
  next: number | null;
}
