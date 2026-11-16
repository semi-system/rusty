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
# The inbound listeners hold messages in memory: their budget follows the container's limit (limits.sh).
blk_limits() { local v; v="$(inflight_of "$1")"; [ -z "$v" ] || printf '[limits]
inflight_message_mib = %s

' "$v"; }
head_common() { printf 'hostname = "%s"\nblob_dir = "/var/lib/rusty/blobs"\n\n' "$MAIL_HOST"; }

conf_smtp_in() { head_common; printf '[smtp]\nlisten_25 = "0.0.0.0:2525"\nproxy_protocol = true\ntrusted_proxies = ["127.0.0.1/32"]\ntrusted_proxy_hosts = ["tasks.edge", "edge"]\n\n'; blk_tls; blk_unseal; blk_blob; blk_store smtp_in; blk_limits smtp-in; blk_metrics; }
conf_submission() { head_common; printf '[smtp]\nlisten_587 = "0.0.0.0:5587"\nlisten_465 = "0.0.0.0:4465"\nproxy_protocol = true\ntrusted_proxies = ["127.0.0.1/32"]\ntrusted_proxy_hosts = ["tasks.edge", "edge"]\n\n[outbound]\npublic_ips = ["%s"]\n\n' "$(manager_public_ip)"; blk_tls; blk_unseal; blk_blob; blk_store submission; blk_limits submission; blk_metrics; }
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

# ---- the defaults: PostgreSQL and RustFS inside the stack when the inventory names none -----------------------------------------------------------------------------------------------------
# DB_HOST empty: a PostgreSQL 18 service pinned to the node labelled db=true, TLS on with a CA made here, its superuser password made here and kept in the state directory (yours, for manual
# backups and repairs). S3_ENDPOINT empty: a RustFS service pinned to the node labelled s3=true with keys made here. Each is ONE instance on ONE node with its data in a local volume: no failover.
DB_IN_STACK=0; S3_IN_STACK=0
[ -n "$DB_HOST" ] || DB_IN_STACK=1
[ -n "$S3_ENDPOINT" ] || S3_IN_STACK=1
STACK_ONLY=""

rand_hex() { od -An -N"$1" -tx1 /dev/urandom | tr -d ' \n'; }

# shellcheck disable=SC2034 # the values are read by rusty-swarm, which sources this file
resolve_defaults() {
  mkdir -p "$STATE_DIR/secrets"; chmod 0700 "$STATE_DIR" "$STATE_DIR/secrets" 2>/dev/null || true
  if [ "$DB_IN_STACK" = 1 ]; then
    DB_HOST=db; DB_PORT=5432; DB_NAME=rusty; DB_TLS=1; DB_HOSTADDR=""; DB_ADMIN_USER=postgres
    DB_CA="$STATE_DIR/dbtls/ca.pem"; DB_ADMIN_PASSWORD_FILE="$STATE_DIR/secrets/pg_postgres"
    [ -s "$DB_ADMIN_PASSWORD_FILE" ] || ( umask 077; rand_hex 24 > "$DB_ADMIN_PASSWORD_FILE" )
  fi
  if [ "$S3_IN_STACK" = 1 ]; then
    S3_ENDPOINT=http://s3:9000; S3_REGION=us-east-1; S3_BUCKET=rusty-mail
    [ -s "$STATE_DIR/secrets/s3_access_key" ] || ( umask 077; rand_hex 10 > "$STATE_DIR/secrets/s3_access_key" )
    [ -s "$STATE_DIR/secrets/s3_secret_key" ] || ( umask 077; rand_hex 24 > "$STATE_DIR/secrets/s3_secret_key" )
    S3_ACCESS_KEY="$(cat "$STATE_DIR/secrets/s3_access_key")"; S3_SECRET_KEY_FILE="$STATE_DIR/secrets/s3_secret_key"
  fi
}

# The CA and the server certificate of the default PostgreSQL (the name `db`), made once.
make_db_tls() {
  local dir="$STATE_DIR/dbtls" out
  mkdir -p "$dir"; chmod 0700 "$dir"
  if [ -s "$dir/ca.pem" ] && [ -s "$dir/server.crt" ] && [ -s "$dir/server.key" ]; then return 0; fi
  out="$(sw run --rm --cap-drop ALL --entrypoint sh "$POSTGRES_IMAGE" -c 'set -e; cd /tmp
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-384 -nodes -days 3650 -subj "/CN=rusty db CA" -keyout ca.key -out ca.pem 2>/dev/null
    openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-384 -nodes -subj "/CN=db" -keyout server.key -out server.csr 2>/dev/null
    printf "subjectAltName=DNS:db\nextendedKeyUsage=serverAuth\n" > ext.cnf
    openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 3650 -extfile ext.cnf -out server.crt 2>/dev/null
    cat ca.pem; echo ----SPLIT----; cat server.key; echo ----SPLIT----; cat server.crt')"
  ( umask 077
    printf '%s\n' "$out" | awk 'BEGIN { n = 0 } /^----SPLIT----$/ { n++; next } n == 0 { print }' > "$dir/ca.pem"
    printf '%s\n' "$out" | awk 'BEGIN { n = 0 } /^----SPLIT----$/ { n++; next } n == 1 { print }' > "$dir/server.key"
    printf '%s\n' "$out" | awk 'BEGIN { n = 0 } /^----SPLIT----$/ { n++; next } n == 2 { print }' > "$dir/server.crt" )
  grep -q 'BEGIN CERTIFICATE' "$dir/ca.pem" && grep -q 'PRIVATE KEY' "$dir/server.key" && grep -q 'BEGIN CERTIFICATE' "$dir/server.crt" || die "could not make the certificates of the default PostgreSQL"
}

conf_pg_hba() { printf 'local all all trust\nhostssl all all all scram-sha-256\nhost all all all reject\n'; }

gen_db_service() {
  printf '  db:\n    image: %s\n    cap_drop: [ALL]\n    cap_add: [CHOWN, DAC_OVERRIDE, FOWNER, SETGID, SETUID]\n' "$(lock_ref 'postgres:18')"
  printf '    environment:\n      POSTGRES_DB: %s\n      POSTGRES_PASSWORD_FILE: /run/secrets/pg_postgres\n      POSTGRES_INITDB_ARGS: "--auth-host=scram-sha-256 --data-checksums"\n' "$DB_NAME"
  printf '    command: [postgres, -c, ssl=on, -c, ssl_cert_file=/run/secrets/db_server_crt, -c, ssl_key_file=/run/secrets/db_server_key, -c, ssl_min_protocol_version=TLSv1.3, -c, hba_file=/etc/postgresql-hba/pg_hba.conf]\n'
  printf '    volumes:\n      - pgdata:/var/lib/postgresql\n'
  y_deploy 1 "node.labels.db == true" stop-first db
  printf '    secrets:\n'
  y_secret pg_postgres pg_postgres 0
  y_secret db_server_crt db_server_crt 999
  y_secret db_server_key db_server_key 999
  printf '    configs:\n'
  y_config pg_hba /etc/postgresql-hba/pg_hba.conf
  printf '    healthcheck:\n      test: [CMD-SHELL, "pg_isready -U postgres -d %s"]\n      interval: 5s\n      timeout: 5s\n      retries: 20\n    networks:\n      - backend\n' "$DB_NAME"
}

gen_s3_service() {
  printf '  s3:\n    image: %s\n    read_only: true\n    cap_drop: [ALL]\n    command: ["/data"]\n' "$(lock_ref 'rustfs/rustfs:latest')"
  printf '    environment:\n      RUSTFS_ACCESS_KEY_FILE: /run/secrets/s3_access\n      RUSTFS_SECRET_KEY_FILE: /run/secrets/s3_secret\n'
  printf '    volumes:\n      - s3data:/data\n      - type: tmpfs\n        target: /logs\n        tmpfs: { mode: 1777 }\n      - type: tmpfs\n        target: /tmp\n        tmpfs: { mode: 1777 }\n'
  y_deploy 1 "node.labels.s3 == true" stop-first s3
  printf '    secrets:\n'
  y_secret s3_access s3_access 0
  y_secret s3_secret s3_secret 0
  printf '    networks:\n      - backend\n'
}

# ---- resource limits -------------------------------------------------------------------------------------------------------------------------------------------------------------------------
# limits.sh of the standalone stack divides 80 % of the memory and CPUs of the Ready nodes (read from the Manager) over the services by weight; its answer goes into `deploy.resources` and, for the
# inbound listeners, into `[limits] inflight_message_mib` (which must stay well under the container's memory). Run again after nodes join or leave.
LIMITS_FILE=""
gen_limits() {
  local mem cpus id st av n m profiles="" reps="" s
  mem=0; cpus=0
  while read -r id st av; do
    [ "$st" = Ready ] && [ "$av" = Active ] || continue
    read -r n m < <(sw node inspect "$id" --format '{{.Description.Resources.NanoCPUs}} {{.Description.Resources.MemoryBytes}}')
    cpus=$((cpus + n)); mem=$((mem + m))
  done < <(sw node ls --format '{{.ID}} {{.Status}} {{.Availability}}')
  [ "$mem" -gt 0 ] || die "limits: no Ready and Active node"
  [ "$DB_IN_STACK" = 1 ] || profiles="nodb"
  [ "$S3_IN_STACK" = 0 ] || profiles="${profiles:+$profiles,}s3"
  if [ "$REPLICAS_DEFAULT" != 1 ]; then for s in smtp-in submission imap outbound gc admin; do reps="${reps:+$reps,}$s=$REPLICAS_DEFAULT"; done; fi
  LIMITS_FILE="$STATE_DIR/stack/limits.yaml"
  bash "$HERE/../standalone/limits.sh" --total-mem-mib "$((mem / 1048576))" --total-cpus "$(awk -v n="$cpus" 'BEGIN { printf "%.2f", n / 1000000000 }')" ${profiles:+--profiles "$profiles"} ${reps:+--replicas "$reps"} \
    --out "$LIMITS_FILE" > "$STATE_DIR/stack/limits.txt" || { cat "$STATE_DIR/stack/limits.txt" >&2; die "limits: the services do not fit in the nodes (see above)"; }
}
# The memory ceiling of the inbound listeners' in-flight messages for a service, from the table limits.sh printed (empty when there is none).
inflight_of() { awk -v n="$1" '$1 == n { for (i = 1; i <= NF; i++) if ($i == "<=") print $(i + 1) }' "$STATE_DIR/stack/limits.txt" 2>/dev/null; }
# The `resources` lines of a service in the file limits.sh wrote (nothing when the service has none).
y_limits() { [ -s "$STATE_DIR/stack/limits.yaml" ] && awk -v n="  $1:" '$0 == n { f = 1; next } f && /limits:/ { sub(/^ +/, ""); print "      resources:"; print "        " $0; exit } f && /^  [a-z]/ { exit }' "$STATE_DIR/stack/limits.yaml"; return 0; }

# ---- prepare secrets and prepare configs --------------------------------------------------------------------------------------------------------------------------------------------------
require_stack_inputs() {
  resolve_defaults
  [ -n "$MAIL_HOST" ] && [ -n "$ADMIN_HOST" ] && [ -n "$ADMIN_ALLOW" ] || die "the inventory needs MAIL_HOST, ADMIN_HOST and ADMIN_ALLOW"
  [ -n "$S3_ACCESS_KEY" ] && [ -r "$S3_SECRET_KEY_FILE" ] || die "S3_ENDPOINT is set but S3_ACCESS_KEY or S3_SECRET_KEY_FILE is not"
  [ "$TLS_MODE" = self ] || die "TLS_MODE=$TLS_MODE: only self (self-signed certificates) is built so far; ACME and the certificate sync are a later step"
  make_passwords
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
  if [ "$DB_IN_STACK" = 1 ]; then
    make_db_tls
    ensure_secret pg_postgres "$DB_ADMIN_PASSWORD_FILE"
    ensure_secret db_server_crt "$STATE_DIR/dbtls/server.crt"
    ensure_secret db_server_key "$STATE_DIR/dbtls/server.key"
  fi
  echo "prepare secrets: done"
}

cmd_prepare_configs() {
  require_stack_inputs
  [ -s "$STATE_DIR/keys/seal.pub" ] || die "no sealing key yet: run 'prepare secrets' first"
  local d="$STATE_DIR/stack/etc" s
  mkdir -p "$d"; chmod 0700 "$STATE_DIR/stack" "$d"
  gen_limits
  for s in smtp_in submission imap outbound gc admin migrate; do
    ( umask 077; "conf_$s" > "$d/$s.toml" )
    ensure_config "$s" "$d/$s.toml"
  done
  ( umask 077; conf_haproxy > "$d/haproxy.cfg" )
  ensure_config haproxy "$d/haproxy.cfg"
  if [ "$DB_IN_STACK" = 1 ]; then make_db_tls; ( umask 077; conf_pg_hba > "$d/pg_hba.conf" ); ensure_config pg_hba "$d/pg_hba.conf"; fi
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
  if [ "$DB_IN_STACK" = 1 ]; then
    for base in pg_postgres db_server_crt db_server_key; do
      n="$(sw secret ls --filter "label=rusty.base=$base" --format '{{.Name}}' | sed "s/^${base}_v//" | sort -n | tail -1)"
      [ -n "$n" ] || die "the secret $base is missing: run 'prepare secrets'"
      SECRET_NAME[$base]="${base}_v$n"
    done
    n="$(sw config ls --filter "label=rusty.base=pg_hba" --format '{{.Name}} {{.CreatedAt}}' | sort -k2 | tail -1 | cut -d' ' -f1)"
    [ -n "$n" ] || die "the config pg_hba is missing: run 'prepare configs'"
    CONFIG_NAME[pg_hba]="$n"
  fi
  for base in smtp_in submission imap outbound gc admin migrate haproxy db_ca; do
    n="$(sw config ls --filter "label=rusty.base=$base" --format '{{.Name}} {{.CreatedAt}}' | sort -k2 | tail -1 | cut -d' ' -f1)"
    [ -n "$n" ] || { [ "$base" = db_ca ] && continue; die "the config $base is missing: run 'prepare configs'"; }
    CONFIG_NAME[$base]="$n"
  done
}

y_service_common() { # NAME IMAGE REPLICAS
  printf '  %s:\n    image: %s\n    read_only: true\n    cap_drop: [ALL]\n    stop_grace_period: 45s\n    volumes:\n      - type: tmpfs\n        target: /tmp\n' "$1" "$2"
}
y_deploy() { # REPLICAS [CONSTRAINT] [ORDER] [SERVICE: its resource limits]
  printf '    deploy:\n      replicas: %s\n      restart_policy: { condition: any, delay: 5s }\n      update_config: { parallelism: 1, order: %s, delay: 10s, monitor: 20s, failure_action: rollback }\n      rollback_config: { parallelism: 1, order: %s }\n' "$1" "${3:-start-first}" "${3:-start-first}"
  [ -z "${2:-}" ] || printf '      placement:\n        constraints: ["%s"]\n' "$2"
  [ -z "${4:-}" ] || y_limits "$4"
}
y_hosts() { [ -z "$DB_HOSTADDR" ] || printf '    extra_hosts:\n      - "%s:%s"\n' "$DB_HOST" "$DB_HOSTADDR"; }

gen_rusty_service() { # NAME ROLE-OF-DB NETWORKS(space) [TLS] [UNSEAL] [CONSTRAINT]
  local name="$1" role="$2" nets="$3" tls="${4:-}" unseal="${5:-}" constraint="${6:-}" n key
  y_service_common "$name" "$(rusty_image "$name")"
  y_deploy "$REPLICAS_DEFAULT" "$constraint" start-first "$name"
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
  gen_limits
  load_names
  {
    printf 'version: "3.9"\nservices:\n'
    [ "$DB_IN_STACK" = 0 ] || gen_db_service
    [ "$S3_IN_STACK" = 0 ] || gen_s3_service
    if [ -z "$STACK_ONLY" ]; then
      # The edge: the one public entrance, on the Manager, its ports on the host (no routing mesh: the clients' addresses are kept).
      y_service_common edge "$(lock_ref 'haproxy:3.4.7-alpine3.24')"
      printf '    sysctls: { net.ipv4.ip_unprivileged_port_start: 0 }\n    ports:\n'
      local p
      for p in 25 80 143 443 465 587 993; do printf '      - { target: %s, published: %s, protocol: tcp, mode: host }\n' "$p" "$p"; done
      y_deploy 1 "node.role == manager" stop-first edge
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
    fi
    printf 'networks:\n  app: { driver: overlay, internal: true }\n  backend: { driver: overlay, attachable: true }\n  metrics: { driver: overlay, internal: true }\n'
    if [ "$DB_IN_STACK" = 1 ] || [ "$S3_IN_STACK" = 1 ]; then
      printf 'volumes:\n'
      [ "$DB_IN_STACK" = 0 ] || printf '  pgdata:\n'
      [ "$S3_IN_STACK" = 0 ] || printf '  s3data:\n'
    fi
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
    printf '    networks: [backend]\n'
    if [ "$S3_IN_STACK" = 1 ]; then
      # The default S3 server has no bucket yet: the one-shot `blobs init` makes it (with [blob_s3] it makes the bucket, not the directory).
      printf '  blobs-init:\n    image: %s\n    entrypoint: [/usr/local/bin/rustyctl]\n    command: [--config, /etc/rusty/migrate.toml, blobs, init]\n    read_only: true\n    cap_drop: [ALL]\n    deploy:\n      mode: replicated-job\n      restart_policy: { condition: none }\n    secrets:\n' "$(rusty_image "")"
      y_secret s3_access; y_secret s3_secret; y_secret pg_gc
      printf '    configs:\n'
      y_config migrate /etc/rusty/migrate.toml
      if [ -n "${CONFIG_NAME[db_ca]:-}" ]; then y_config db_ca /etc/rusty/db-ca.pem; fi
      printf '    networks: [backend]\n'
    fi
    if [ "$DB_IN_STACK" = 1 ] || [ "$S3_IN_STACK" = 1 ]; then
      # The default PostgreSQL and S3 are on the stack's own network: the jobs join it.
      printf 'networks:\n  backend: { external: true, name: %s_backend }\n' "$STACK_NAME"
    else
      printf 'networks:\n  backend: { driver: overlay, attachable: true }\n'
    fi
    gen_externals_for_migrate
  }
}
gen_externals_for_migrate() {
  local k
  printf 'secrets:\n'
  for k in pg_app pg_migrator pg_gc s3_access s3_secret; do printf '  %s: { external: true }\n' "${SECRET_NAME[$k]}"; done
  printf 'configs:\n  %s: { external: true }\n' "${CONFIG_NAME[migrate]}"
  [ -z "${CONFIG_NAME[db_ca]:-}" ] || printf '  %s: { external: true }\n' "${CONFIG_NAME[db_ca]}"
}

# ---- migrate and deploy ------------------------------------------------------------------------------------------------------------------------------------------------------------------
cmd_migrate() {
  local jobs=(migrate) j state ndone failed
  [ "$S3_IN_STACK" = 0 ] || jobs+=(blobs-init)
  gen_migrate_stack > "$STATE_DIR/stack/migrate.yaml"
  sw stack rm "$MIGRATE_STACK" >/dev/null 2>&1 || true
  for _ in $(seq 1 30); do [ -z "$(sw service ls --filter "label=com.docker.stack.namespace=$MIGRATE_STACK" -q)" ] && break; sleep 1; done
  sw stack deploy --with-registry-auth -c - "$MIGRATE_STACK" < "$STATE_DIR/stack/migrate.yaml" >/dev/null
  for _ in $(seq 1 90); do
    ndone=0; failed=""
    for j in "${jobs[@]}"; do
      state="$(sw service ps "${MIGRATE_STACK}_$j" --format '{{.CurrentState}}' 2>/dev/null | head -1)"
      case "$state" in
        Complete*) ndone=$((ndone + 1)) ;;
        Failed*|Rejected*) failed="$failed $j" ;;
      esac
    done
    if [ -n "$failed" ]; then
      for j in $failed; do echo "migrate: the job $j failed; its log:" >&2; sw service logs "${MIGRATE_STACK}_$j" 2>&1 | tail -20 >&2; done
      sw stack rm "$MIGRATE_STACK" >/dev/null 2>&1 || true
      die "a job failed: nothing was deployed"
    fi
    if [ "$ndone" = "${#jobs[@]}" ]; then echo "migrate: the schema is up to date${S3_IN_STACK/#1/ and the bucket exists}"; sw stack rm "$MIGRATE_STACK" >/dev/null 2>&1 || true; return 0; fi
    sleep 2
  done
  for j in "${jobs[@]}"; do sw service logs "${MIGRATE_STACK}_$j" 2>&1 | tail -20 >&2 || true; done
  die "the jobs did not finish in time"
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

# ---- backup, restore, upgrade ----------------------------------------------------------------------------------------------------------------------------------------------------------------
# A dump of the database with the login that stays yours (pg_dump -Fc, from a container on a node: the same road the services take) and the sealing key. The S3 bucket is not in it: it is yours
# (or, with the default RustFS, the volume `s3data` on the node labelled s3=true): copy it the way you copy any bucket.
# shellcheck disable=SC2120 # called without a directory by upgrade
cmd_backup() { # [DIR]
  local dir
  dir="${1:-$STATE_DIR/backups}/$(date -u +%Y%m%dT%H%M%SZ)"
  [ -r "$DB_ADMIN_PASSWORD_FILE" ] || die "backup: DB_ADMIN_PASSWORD_FILE is not readable"
  ( umask 077; mkdir -p "$dir" )
  ( umask 077; pg_tool_as "pg_dump -Fc --no-owner" "$DB_ADMIN_USER" "$DB_ADMIN_PASSWORD_FILE" < /dev/null > "$dir/rusty.dump" ) || { rm -rf "$dir"; die "backup: pg_dump failed"; }
  [ -s "$dir/rusty.dump" ] || { rm -rf "$dir"; die "backup: the dump is empty"; }
  [ ! -d "$STATE_DIR/keys" ] || ( umask 077; cp -R "$STATE_DIR/keys" "$dir/keys" )
  echo "backup: $dir ($(wc -c < "$dir/rusty.dump") bytes of database, and the sealing key). The S3 bucket is not in it."
}

cmd_restore() { # DUMP-FILE: into the database of the inventory; the services should be stopped (docker stack rm rusty) while it runs
  local f="${1:?usage: restore FILE (a rusty.dump made by backup)}"
  [ -s "$f" ] || die "restore: $f is not a file"
  [ -r "$DB_ADMIN_PASSWORD_FILE" ] || die "restore: DB_ADMIN_PASSWORD_FILE is not readable"
  pg_tool_as "pg_restore --clean --if-exists --no-owner -d" "$DB_ADMIN_USER" "$DB_ADMIN_PASSWORD_FILE" < "$f" || die "restore: pg_restore reported errors (see above)"
  echo "restore: done; run 'prepare db' (the roles' rights), 'migrate' and 'deploy'"
}

# A new release: unpack its kit over the same swarm.env and STATE_DIR and run this. A backup first (a migration is forward only: the way back is a restore), then the secrets and configs that
# changed, the migration job (on the new image, before any service moves), then a rolling update that goes back by itself when a service does not come up.
cmd_upgrade() {
  local backup=1
  while [ $# -gt 0 ]; do case "$1" in --no-backup) backup=0; shift ;; *) die "upgrade: unknown argument $1 (only --no-backup)" ;; esac; done
  [ "$backup" = 0 ] || cmd_backup
  cmd_prepare_secrets
  cmd_prepare_configs
  cmd_migrate
  cmd_deploy
  echo "upgrade: done"
}
