#!/usr/bin/env bash
# The stack of the swarm form (docs/design-swarm.md, sections 6 and 11): the secrets and configs the services read, the migration job, and `stack.yaml`. Sourced by rusty-swarm; not run on its own.
#
# What is generated is plain text from the inventory (swarm.env): no service has an address in it. The services find each other by NAME (the edge as `tasks.edge`, the admin web as `tasks.admin`);
# secrets are named with a version (`pg_imap_v1`: a secret cannot be changed in place, a new content is a new version) and configs by the hash of their content, so a change is a rolling update.

[ "${BASH_VERSINFO[0]}" -ge 4 ] || die "this tool needs bash 4 or newer"

STACK_NAME=rusty
MIGRATE_STACK=rustymigrate
REGISTRY="${REGISTRY:-ghcr.io/semi-system}"
IMAGE_TAG="${IMAGE_TAG:-main}"
TLS_MODE="${TLS_MODE:-self}"
REPLICAS_DEFAULT="${REPLICAS_DEFAULT:-1}"
declare -A SECRET_NAME=() CONFIG_NAME=()

sw() { dk "$(manager_name)" "$@"; }
sha16() { if command -v sha256sum >/dev/null 2>&1; then sha256sum | cut -c1-16; else shasum -a 256 | cut -c1-16; fi; }
manager_public_ip() { local m; m="$(manager_name)"; node_addr "$m"; }

# ---- images --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------
# A kit's images.lock names the images of its release by digest; a checkout has none, and the tag of IMAGE_TAG (default `main`) is used.
lock_ref() { awk -v k="$1" '$1 == k { print $2; exit }' "$HERE/../images.lock"; }
rusty_image() { # NAME (rusty, smtp-in, ...)
  local name="rusty${1:+-$1}" pinned
  [ "$1" != "" ] || name=rusty
  pinned="$(awk -v p="$REGISTRY/$name:" 'index($1, p) == 1 { print $2; exit }' "$HERE/../images.lock")"
  if [ -n "$pinned" ]; then printf '%s\n' "$pinned"; else printf '%s/%s:%s\n' "$REGISTRY" "$name" "$IMAGE_TAG"; fi
}

# ---- secrets and configs ---------------------------------------------------------------------------------------------------------------------------------------------------------------------
ensure_secret() { # BASE FILE -> the versioned name (made when the content is new)
  local base="$1" f="$2" sha cur n
  [ -s "$f" ] || die "the secret $base has no content ($f)"
  sha="$(sha16 < "$f")"
  cur="$(sw secret ls --filter "label=rusty.base=$base" --filter "label=rusty.sha=$sha" --format '{{.Name}}' | head -1)"
  if [ -z "$cur" ]; then
    n="$(sw secret ls --filter "label=rusty.base=$base" --format '{{.Name}}' | sed "s/^${base}_v//" | sort -n | tail -1)"
    cur="${base}_v$(( ${n:-0} + 1 ))"
    sw secret create --label "rusty.base=$base" --label "rusty.sha=$sha" "$cur" - < "$f" >/dev/null
    echo "  created secret $cur" >&2
  fi
  SECRET_NAME[$base]="$cur"
}

ensure_config() { # BASE FILE -> the name made of the content hash
  local base="$1" f="$2" sha name
  sha="$(sha16 < "$f")"
  name="${base}_$sha"
  if ! sw config inspect "$name" >/dev/null 2>&1; then
    sw config create --label "rusty.base=$base" "$name" - < "$f" >/dev/null
    echo "  created config $name" >&2
  fi
  CONFIG_NAME[$base]="$name"
}

# The self-signed certificate of a host name (the first start, and a test installation): kept in the state directory, made again when the name changes.
make_selfsigned() { # NAME HOST
  local name="$1" host="$2" dir="$STATE_DIR/tls" out
  mkdir -p "$dir"; chmod 0700 "$dir"
  if [ -s "$dir/$name.crt" ] && [ -s "$dir/$name.key" ] && [ "$(cat "$dir/$name.host" 2>/dev/null)" = "$host" ]; then return 0; fi
  out="$(sw run --rm --cap-drop ALL --entrypoint sh "$POSTGRES_IMAGE" -c "cd /tmp && openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 825 -subj '/CN=$host' -addext 'subjectAltName=DNS:$host' -keyout k.pem -out c.pem 2>/dev/null && cat k.pem && echo ----SPLIT---- && cat c.pem")"
  ( umask 077
    printf '%s\n' "$out" | awk '/^----SPLIT----$/ { exit } { print }' > "$dir/$name.key"
    printf '%s\n' "$out" | awk 'f { print } /^----SPLIT----$/ { f = 1 }' > "$dir/$name.crt"
    cat "$dir/$name.key" "$dir/$name.crt" > "$dir/$name.pem"
    printf '%s' "$host" > "$dir/$name.host" )
  [ -s "$dir/$name.crt" ] && grep -q 'BEGIN CERTIFICATE' "$dir/$name.crt" || die "could not make a certificate for $host"
}

# The sealing key pair (docs/blob-stores.md): made once; the private half is kept in the state directory and goes to the services that open blobs; print the path once for a backup.
make_sealing_key() {
  local dir="$STATE_DIR/keys" tmp
  mkdir -p "$dir"; chmod 0700 "$dir"
  if [ -s "$dir/unseal.key" ] && [ -s "$dir/seal.pub" ]; then return 0; fi
  tmp="/tmp/rusty-swarm-keys.$$"
  on "$(manager_name)" sh -c "rm -rf $tmp && mkdir -m 1777 $tmp"
  sw run --rm --user 65532:65532 --entrypoint /usr/local/bin/rustyctl -v "$tmp:/k" "$(rusty_image "")" seal keygen /k/unseal.key >/dev/null
  ( umask 077
    on "$(manager_name)" cat "$tmp/unseal.key" > "$dir/unseal.key"
    sw run --rm --user 65532:65532 --entrypoint /usr/local/bin/rustyctl -v "$tmp:/k:ro" "$(rusty_image "")" seal show /k/unseal.key | grep -o 'rusty-seal-v1:[0-9a-f]\{64\}' | head -1 > "$dir/seal.pub" )
  on "$(manager_name)" rm -rf "$tmp"
  [ -s "$dir/unseal.key" ] && [ -s "$dir/seal.pub" ] || die "could not make the sealing key"
  echo "prepare secrets: a sealing key was made: $dir/unseal.key. It is the key that opens the tenants' stored credentials: keep a copy somewhere else (without it they cannot be read)." >&2
}

# ---- the configuration files of the services ---------------------------------------------------------------------------------------------------------------------------------------------
db_ssl_lines() {
  if [ "$DB_TLS" = 1 ]; then
    echo 'sslmode = "verify-full"'
    [ -z "$DB_CA" ] || echo 'ca_bundle = "/etc/rusty/db-ca.pem"'
  else
    echo 'sslmode = "disable"'
    echo 'insecure_allow_unverified_tls = true'
  fi
}
blk_store() { # ROLE
  printf '[store]\nurl = "postgres://rusty_%s@%s:%s/%s"\npassword_file = "/run/secrets/pg_%s"\n%s\n\n' "$1" "$DB_HOST" "$DB_PORT" "$DB_NAME" "$1" "$(db_ssl_lines)"
}
blk_blob() {
  printf '[blob_s3]\nendpoint = "%s"\nregion = "%s"\nbucket = "%s"\npath_style = true\naccess_key_file = "/run/secrets/s3_access"\nsecret_key_file = "/run/secrets/s3_secret"\n\n' "$S3_ENDPOINT" "$S3_REGION" "$S3_BUCKET"
}
blk_unseal() { printf '[unsealing]\nkey_files = ["/etc/rusty-keys/unseal.key"]\n\n'; }
blk_tls() { printf '[tls]\ncert_file = "/etc/rusty-tls/cert.pem"\nkey_file = "/etc/rusty-tls/key.pem"\n\n'; }
blk_metrics() { printf '[metrics]\nlisten = "0.0.0.0:9100"\nallowed_hosts = ["tasks.admin", "admin"]\n\n'; }
head_common() { printf 'hostname = "%s"\nblob_dir = "/var/lib/rusty/blobs"\n\n' "$MAIL_HOST"; }

conf_smtp_in() { head_common; printf '[smtp]\nlisten_25 = "0.0.0.0:2525"\nproxy_protocol = true\ntrusted_proxies = ["127.0.0.1/32"]\ntrusted_proxy_hosts = ["tasks.edge", "edge"]\n\n'; blk_tls; blk_unseal; blk_blob; blk_store smtp_in; blk_metrics; }
conf_submission() { head_common; printf '[smtp]\nlisten_587 = "0.0.0.0:5587"\nlisten_465 = "0.0.0.0:4465"\nproxy_protocol = true\ntrusted_proxies = ["127.0.0.1/32"]\ntrusted_proxy_hosts = ["tasks.edge", "edge"]\n\n[outbound]\npublic_ips = ["%s"]\n\n' "$(manager_public_ip)"; blk_tls; blk_unseal; blk_blob; blk_store submission; blk_metrics; }
conf_imap() { head_common; printf '[imap]\nlisten_143 = "0.0.0.0:1143"\nlisten_993 = "0.0.0.0:1993"\nproxy_protocol = true\ntrusted_proxies = ["127.0.0.1/32"]\ntrusted_proxy_hosts = ["tasks.edge", "edge"]\n\n'; blk_tls; blk_unseal; blk_blob; blk_store imap; blk_metrics; }
conf_outbound() { head_common; printf '[outbound]\ndkim_dir = "/var/lib/rusty/dkim"\npublic_ips = ["%s"]\n\n' "$(manager_public_ip)"; blk_unseal; blk_blob; blk_store outbound; blk_metrics; }
conf_gc() { head_common; printf 'gc_grace_seconds = 604800\n\n[dns_check]\nenabled = true\n\n'; blk_unseal; blk_blob; blk_store gc; blk_metrics; }
conf_admin() {
  printf 'hostname = "%s"\n\n[admin]\nlisten = "0.0.0.0:8080"\npublic_origin = "https://%s"\npublic_ips = ["%s"]\nallowed_peers = ["127.0.0.1/32"]\nallowed_hosts = ["tasks.edge", "edge"]\n\n' "$MAIL_HOST" "$ADMIN_HOST" "$(manager_public_ip)"
  blk_store admin
  printf '[sealing]\npublic_key = "%s"\n\n' "$(cat "$STATE_DIR/keys/seal.pub")"
  if [ -n "${METRICS_ALLOW:-}" ]; then
    local c list=""
    for c in $METRICS_ALLOW; do list="${list:+$list, }\"$c\""; done
    printf '[metrics]\nlisten = "0.0.0.0:9100"\nallowed_peers = [%s]\n\n' "$list"
    for c in smtp-in submission imap outbound gc; do printf '[[metrics.scrape]]\nservice = "%s"\naddress = "tasks.%s:9100"\n\n' "$c" "$c"; done
  fi
}
conf_migrate() { head_common; printf 'gc_grace_seconds = 604800\n\n'; blk_blob; blk_store gc; printf 'migrator_user = "rusty_migrator"\nmigrator_password_file = "/run/secrets/pg_migrator"\n'; }

# The edge's configuration: the standalone template with the swarm's backends (every task of a service by `tasks.<service>`) and the certificates it has (self-signed here).
conf_haproxy() {
  local allow="$ADMIN_ALLOW"
  sed -e '/^#ACME/d' -e 's/^#SELF //' \
      -e 's/\(server-template [a-z-]* [0-9]-[0-9]\) \([a-z-]*\):/\1 tasks.\2:/' \
      -e "s|@MAIL_HOST@|$MAIL_HOST|g" -e "s|@ADMIN_HOST@|$ADMIN_HOST|g" -e "s|@ADMIN_ALLOW@|$allow|g" -e "s|@CONTACT@|${CONTACT:-}|g" -e "s|@ACME_DIRECTORY@||g" \
      "$HERE/../standalone/templates/haproxy.cfg.in"
}

# ---- prepare secrets and prepare configs --------------------------------------------------------------------------------------------------------------------------------------------------
require_stack_inputs() {
  [ -n "$MAIL_HOST" ] && [ -n "$ADMIN_HOST" ] && [ -n "$ADMIN_ALLOW" ] || die "the inventory needs MAIL_HOST, ADMIN_HOST and ADMIN_ALLOW"
  [ -n "$DB_HOST" ] || die "the default PostgreSQL of the stack is a later step: set DB_HOST (an external PostgreSQL)"
  [ -n "$S3_ENDPOINT" ] && [ -r "$S3_SECRET_KEY_FILE" ] && [ -n "$S3_ACCESS_KEY" ] || die "the default RustFS of the stack is a later step: set S3_ENDPOINT, S3_ACCESS_KEY and S3_SECRET_KEY_FILE (an external S3)"
  [ "$TLS_MODE" = self ] || die "TLS_MODE=$TLS_MODE: only self (self-signed certificates) is built so far; ACME and the certificate sync are a later step"
  [ -s "$(secret_file app)" ] || die "no service passwords yet: run 'prepare db' first"
}

cmd_prepare_secrets() {
  require_stack_inputs
  local r d="$STATE_DIR/stack"
  mkdir -p "$d"; chmod 0700 "$d"
  make_sealing_key
  make_selfsigned mail "$MAIL_HOST"
  make_selfsigned admin "$ADMIN_HOST"
  for r in "${SERVICE_ROLES[@]}"; do ensure_secret "pg_$r" "$(secret_file "$r")"; done
  ( umask 077; printf '%s' "$S3_ACCESS_KEY" > "$d/s3_access" )
  ensure_secret s3_access "$d/s3_access"
  ensure_secret s3_secret "$S3_SECRET_KEY_FILE"
  ensure_secret unseal_key "$STATE_DIR/keys/unseal.key"
  ensure_secret tls_cert "$STATE_DIR/tls/mail.crt"
  ensure_secret tls_key "$STATE_DIR/tls/mail.key"
  ensure_secret edge_mail_pem "$STATE_DIR/tls/mail.pem"
  ensure_secret edge_admin_pem "$STATE_DIR/tls/admin.pem"
  echo "prepare secrets: done"
}

cmd_prepare_configs() {
  require_stack_inputs
  [ -s "$STATE_DIR/keys/seal.pub" ] || die "no sealing key yet: run 'prepare secrets' first"
  local d="$STATE_DIR/stack/etc" s
  mkdir -p "$d"; chmod 0700 "$STATE_DIR/stack" "$d"
  for s in smtp_in submission imap outbound gc admin migrate; do
    ( umask 077; "conf_$s" > "$d/$s.toml" )
    ensure_config "$s" "$d/$s.toml"
  done
  ( umask 077; conf_haproxy > "$d/haproxy.cfg" )
  ensure_config haproxy "$d/haproxy.cfg"
  if [ -n "$DB_CA" ]; then ensure_config db_ca "$DB_CA"; fi
  echo "prepare configs: done"
}

# ---- the stack file ------------------------------------------------------------------------------------------------------------------------------------------------------------------------
# One block per service: a hardened service (read-only root, no capabilities, a private /tmp) with the secrets and configs it names; the names are those made above.
y_secret() { # BASE [TARGET] [UID]
  printf '      - source: %s\n        target: %s\n        uid: "%s"\n        mode: 0400\n' "${SECRET_NAME[$1]}" "${2:-$1}" "${3:-65532}"
}
y_config() { # BASE TARGET
  printf '      - source: %s\n        target: %s\n        mode: 0444\n' "${CONFIG_NAME[$1]}" "$2"
}

load_names() { # the versioned names, from the manager
  local base n
  for base in "${SERVICE_ROLES[@]/#/pg_}" s3_access s3_secret unseal_key tls_cert tls_key edge_mail_pem edge_admin_pem; do
    n="$(sw secret ls --filter "label=rusty.base=$base" --format '{{.Name}}' | sed "s/^${base}_v//" | sort -n | tail -1)"
    [ -n "$n" ] || die "the secret $base is missing: run 'prepare secrets'"
    SECRET_NAME[$base]="${base}_v$n"
  done
  for base in smtp_in submission imap outbound gc admin migrate haproxy db_ca; do
    n="$(sw config ls --filter "label=rusty.base=$base" --format '{{.Name}} {{.CreatedAt}}' | sort -k2 | tail -1 | cut -d' ' -f1)"
    [ -n "$n" ] || { [ "$base" = db_ca ] && continue; die "the config $base is missing: run 'prepare configs'"; }
    CONFIG_NAME[$base]="$n"
  done
}

y_service_common() { # NAME IMAGE REPLICAS
  printf '  %s:\n    image: %s\n    read_only: true\n    cap_drop: [ALL]\n    stop_grace_period: 45s\n    volumes:\n      - type: tmpfs\n        target: /tmp\n' "$1" "$2"
}
y_deploy() { # REPLICAS [CONSTRAINT] [ORDER]
  printf '    deploy:\n      replicas: %s\n      restart_policy: { condition: any, delay: 5s }\n      update_config: { parallelism: 1, order: %s, delay: 10s, monitor: 20s, failure_action: rollback }\n      rollback_config: { parallelism: 1, order: %s }\n' "$1" "${3:-start-first}" "${3:-start-first}"
  [ -z "${2:-}" ] || printf '      placement:\n        constraints: ["%s"]\n' "$2"
}
y_hosts() { [ -z "$DB_HOSTADDR" ] || printf '    extra_hosts:\n      - "%s:%s"\n' "$DB_HOST" "$DB_HOSTADDR"; }

gen_rusty_service() { # NAME ROLE-OF-DB NETWORKS(space) [TLS] [UNSEAL] [CONSTRAINT]
  local name="$1" role="$2" nets="$3" tls="${4:-}" unseal="${5:-}" constraint="${6:-}" n key
  y_service_common "$name" "$(rusty_image "$name")"
  y_deploy "$REPLICAS_DEFAULT" "$constraint"
  y_hosts
  printf '    secrets:\n'
  y_secret "pg_$role"
  if [ "$name" != admin ]; then y_secret s3_access; y_secret s3_secret; fi
  if [ "$unseal" = 1 ]; then y_secret unseal_key /etc/rusty-keys/unseal.key; fi
  if [ "$tls" = 1 ]; then y_secret tls_cert /etc/rusty-tls/cert.pem; y_secret tls_key /etc/rusty-tls/key.pem; fi
  printf '    configs:\n'
  key="${name//-/_}"
  y_config "$key" "/etc/rusty/$name.toml"
  if [ -n "${CONFIG_NAME[db_ca]:-}" ]; then y_config db_ca /etc/rusty/db-ca.pem; fi
  printf '    networks:\n'
  for n in $nets; do printf '      - %s\n' "$n"; done
}

gen_stack() {
  load_names
  {
    printf 'version: "3.9"\nservices:\n'
    # The edge: the one public entrance, on the Manager, its ports on the host (no routing mesh: the clients' addresses are kept).
    y_service_common edge "$(lock_ref 'haproxy:3.4.7-alpine3.24')"
    printf '    sysctls: { net.ipv4.ip_unprivileged_port_start: 0 }\n    ports:\n'
    local p
    for p in 25 80 143 443 465 587 993; do printf '      - { target: %s, published: %s, protocol: tcp, mode: host }\n' "$p" "$p"; done
    y_deploy 1 'node.role == manager' stop-first
    printf '    secrets:\n'
    y_secret edge_mail_pem /var/lib/haproxy/certs/mail.pem 99
    y_secret edge_admin_pem /var/lib/haproxy/certs/admin.pem 99
    printf '    configs:\n'
    y_config haproxy /usr/local/etc/haproxy/haproxy.cfg
    printf '    networks:\n      - app\n      - backend\n'
    gen_rusty_service smtp-in smtp_in "app backend metrics" 1 1
    gen_rusty_service submission submission "app backend metrics" 1 1
    gen_rusty_service imap imap "app backend metrics" 1 1
    gen_rusty_service outbound outbound "backend metrics" 0 1 'node.labels.outbound == true'
    gen_rusty_service gc gc "backend metrics" 0 1
    gen_rusty_service admin admin "app backend metrics" 0 0
    printf 'networks:\n  app: { driver: overlay, internal: true }\n  backend: { driver: overlay }\n  metrics: { driver: overlay, internal: true }\n'
    gen_externals
  }
}

# The secrets and configs the stack uses, as external objects (made by prepare).
gen_externals() {
  local k
  printf 'secrets:\n'
  for k in "${!SECRET_NAME[@]}"; do printf '  %s: { external: true }\n' "${SECRET_NAME[$k]}"; done | sort
  printf 'configs:\n'
  for k in "${!CONFIG_NAME[@]}"; do printf '  %s: { external: true }\n' "${CONFIG_NAME[$k]}"; done | sort
}

# The migration: a job (replicated-job) in a stack of its own, run before the services move; it must finish well before the stack is deployed.
gen_migrate_stack() {
  load_names
  {
    printf 'version: "3.9"\nservices:\n  migrate:\n    image: %s\n    entrypoint: [/usr/local/bin/rustyctl]\n    command: [--config, /etc/rusty/migrate.toml, db, migrate]\n    read_only: true\n    cap_drop: [ALL]\n' "$(rusty_image "")"
    y_hosts
    printf '    deploy:\n      mode: replicated-job\n      restart_policy: { condition: none }\n    secrets:\n'
    y_secret pg_app; y_secret pg_migrator; y_secret s3_access; y_secret s3_secret
    printf '    configs:\n'
    y_config migrate /etc/rusty/migrate.toml
    if [ -n "${CONFIG_NAME[db_ca]:-}" ]; then y_config db_ca /etc/rusty/db-ca.pem; fi
    printf '    networks: [backend]\nnetworks:\n  backend: { driver: overlay }\n'
    gen_externals_for_migrate
  }
}
gen_externals_for_migrate() {
  local k
  printf 'secrets:\n'
  for k in pg_app pg_migrator s3_access s3_secret; do printf '  %s: { external: true }\n' "${SECRET_NAME[$k]}"; done
  printf 'configs:\n  %s: { external: true }\n' "${CONFIG_NAME[migrate]}"
  [ -z "${CONFIG_NAME[db_ca]:-}" ] || printf '  %s: { external: true }\n' "${CONFIG_NAME[db_ca]}"
}

# ---- migrate and deploy ------------------------------------------------------------------------------------------------------------------------------------------------------------------
cmd_migrate() {
  local state
  gen_migrate_stack > "$STATE_DIR/stack/migrate.yaml"
  sw stack rm "$MIGRATE_STACK" >/dev/null 2>&1 || true
  for _ in $(seq 1 30); do [ -z "$(sw service ls --filter "label=com.docker.stack.namespace=$MIGRATE_STACK" -q)" ] && break; sleep 1; done
  sw stack deploy --with-registry-auth -c - "$MIGRATE_STACK" < "$STATE_DIR/stack/migrate.yaml" >/dev/null
  for _ in $(seq 1 90); do
    state="$(sw service ps "${MIGRATE_STACK}_migrate" --format '{{.CurrentState}}' 2>/dev/null | head -1)"
    case "$state" in
      Complete*) echo "migrate: the schema is up to date"; sw stack rm "$MIGRATE_STACK" >/dev/null 2>&1 || true; return 0 ;;
      Failed*|Rejected*) echo "migrate: the job failed ($state); its log:" >&2; sw service logs "${MIGRATE_STACK}_migrate" 2>&1 | tail -20 >&2; sw stack rm "$MIGRATE_STACK" >/dev/null 2>&1 || true; die "the migration failed: nothing was deployed" ;;
    esac
    sleep 2
  done
  sw service logs "${MIGRATE_STACK}_migrate" 2>&1 | tail -20 >&2 || true
  die "the migration did not finish in time"
}

cmd_deploy() {
  gen_stack > "$STATE_DIR/stack/stack.yaml"
  sw stack deploy --with-registry-auth --prune -c - "$STACK_NAME" < "$STATE_DIR/stack/stack.yaml"
  wait_converged
}

# Every service of the stack at its wanted number of replicas, or the first logs of the ones that are not.
wait_converged() {
  local line bad
  for _ in $(seq 1 90); do
    bad=""
    while IFS= read -r line; do
      case "$line" in *" "*) ;; *) continue ;; esac
      local name="${line%% *}" rep="${line#* }"
      [ "${rep%%/*}" = "${rep##*/}" ] || bad="$bad $name($rep)"
    done < <(sw stack services "$STACK_NAME" --format '{{.Name}} {{.Replicas}}' | sed 's/ (.*//' )
    [ -n "$bad" ] || { echo "deploy: every service is at its replicas"; return 0; }
    sleep 4
  done
  echo "deploy: not converged:$bad" >&2
  for line in $bad; do sw service logs --tail 20 "${line%%(*}" 2>&1 | tail -20 >&2 || true; done
  die "the stack did not converge in time"
}

# ---- logs and verify -------------------------------------------------------------------------------------------------------------------------------------------------------------------------
cmd_logs() { # SERVICE [lines]
  local svc="${1:?usage: logs SERVICE [LINES] (for example admin)}" lines="${2:-100}"
  [[ "$svc" =~ ^[a-z][a-z-]*$ ]] || die "logs: a service name such as admin or smtp-in"
  sw service logs --tail "$lines" "${STACK_NAME}_$svc" 2>&1
}

# What the edge shows to the world: the greeting of each mail port, which only comes when the edge reached a service and the service accepted the PROXY header from it (trusted by name).
banner() { # PORT -> the first line the port sends (empty when none)
  on "$(manager_name)" sh -c "if command -v nc >/dev/null 2>&1; then (sleep 2; echo QUIT) | nc -w 6 127.0.0.1 $1; elif command -v bash >/dev/null 2>&1; then bash -c 'exec 3<>/dev/tcp/127.0.0.1/$1; read -r -t 6 l <&3; printf \"%s\n\" \"\$l\"'; fi" 2>/dev/null | head -1 | tr -d '\r'
}

cmd_verify_stack() {
  local bad=0 b
  b="$(banner 25)"; case "$b" in "220 "*) echo "  ok    25: $b" ;; *) echo "  FAIL  25: no SMTP greeting through the edge ('$b')"; bad=1 ;; esac
  b="$(banner 587)"; case "$b" in "220 "*) echo "  ok    587: $b" ;; *) echo "  FAIL  587: no SMTP greeting through the edge ('$b')"; bad=1 ;; esac
  b="$(banner 143)"; case "$b" in "* OK"*) echo "  ok    143: $b" ;; *) echo "  FAIL  143: no IMAP greeting through the edge ('$b')"; bad=1 ;; esac
  [ "$bad" = 0 ] || die "verify: a port does not answer as it should (./rusty-swarm logs edge, logs smtp-in ...)"
  echo "verify: the edge reaches the services and they accept it"
}
