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
TLS_MODE="${TLS_MODE:-acme}"
# Replicas of the Rusty services: two each, so that an update (a rolling update, one task at a time, the new one up before the old one goes) never leaves a service without a task.
# The edge, the database and the object store are one task (the edge holds the host ports, the other two hold the data): updating them stops them for a moment.
# `admin` is one task unless REPLICAS_ADMIN says more: its sessions need the shared key-value store (VALKEY=1) and the one-time setup token and the live-update hub belong to one process.
REPLICAS_DEFAULT="${REPLICAS_DEFAULT:-2}"
REPLICAS_ADMIN="${REPLICAS_ADMIN:-1}"
replicas_of() { case "$1" in admin) echo "$REPLICAS_ADMIN" ;; webmail) if [ "$VALKEY" = 1 ]; then echo "$REPLICAS_DEFAULT"; else echo 1; fi ;; *) echo "$REPLICAS_DEFAULT" ;; esac; }
LOG_LEVEL="${LOG_LEVEL:-info}"
declare -A SECRET_NAME=() CONFIG_NAME=()
# shellcheck source=validate.sh
. "$HERE/validate.sh"
# shellcheck source=profiles.sh
. "$HERE/profiles.sh"
# shellcheck source=edge-prost.sh
. "$HERE/edge-prost.sh"
# shellcheck source=monitoring.sh
. "$HERE/monitoring.sh"

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
  local dir="$STATE_DIR/keys" tmp uid
  mkdir -p "$dir"; chmod 0700 "$dir"
  if [ -s "$dir/unseal.key" ] && [ -s "$dir/seal.pub" ]; then return 0; fi
  tmp="/tmp/rusty-swarm-keys.$$"
  uid="$(on "$(manager_name)" id -u):$(on "$(manager_name)" id -g)"   # the files are made by this user: the tool can read them back
  on "$(manager_name)" sh -c "rm -rf $tmp && mkdir -m 1777 $tmp"
  sw run --rm --user "$uid" --entrypoint /usr/local/bin/rustyctl -v "$tmp:/k" "$(rusty_image "")" seal keygen /k/unseal.key >/dev/null
  ( umask 077
    on "$(manager_name)" cat "$tmp/unseal.key" > "$dir/unseal.key"
    sw run --rm --user "$uid" --entrypoint /usr/local/bin/rustyctl -v "$tmp:/k:ro" "$(rusty_image "")" seal show /k/unseal.key | grep -o 'rusty-seal-v1:[0-9a-f]\{64\}' | head -1 > "$dir/seal.pub" )
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
# LOG_LEVEL (error, warn, info, debug or trace; default info) is the log level of every service: raise it to find out why something is refused (docs/swarm.md).
head_common() {
  printf 'hostname = "%s"\nblob_dir = "/var/lib/rusty/blobs"\n' "$MAIL_HOST"
  [ "$LOG_LEVEL" = info ] || printf 'log_level = "%s"\n' "$LOG_LEVEL"
  printf '\n'
}

conf_smtp_in() { head_common; printf '[smtp]\nlisten_25 = "0.0.0.0:2525"\nproxy_protocol = true\ntrusted_proxies = ["127.0.0.1/32"]\ntrusted_proxy_hosts = ["tasks.edge", "edge"]\n\n'; blk_tls; blk_unseal; blk_blob; blk_filter; blk_store smtp_in; blk_limits smtp-in; blk_metrics; }
conf_submission() { head_common; printf '[smtp]\nlisten_587 = "0.0.0.0:5587"\nlisten_465 = "0.0.0.0:4465"\nproxy_protocol = true\ntrusted_proxies = ["127.0.0.1/32"]\ntrusted_proxy_hosts = ["tasks.edge", "edge"]\n\n[outbound]\npublic_ips = ["%s"]\n\n' "$(manager_public_ip)"; blk_tls; blk_unseal; blk_blob; blk_valkey submission; blk_store submission; blk_limits submission; blk_metrics; }
conf_imap() { head_common; printf '[imap]\nlisten_143 = "0.0.0.0:1143"\nlisten_993 = "0.0.0.0:1993"\nlisten_110 = "0.0.0.0:1110"\nlisten_995 = "0.0.0.0:1995"\nproxy_protocol = true\ntrusted_proxies = ["127.0.0.1/32"]\ntrusted_proxy_hosts = ["tasks.edge", "edge"]\n\n'; blk_tls; blk_unseal; blk_blob; blk_valkey imap; blk_store imap; blk_metrics; }
conf_outbound() { head_common; printf '[outbound]\ndkim_dir = "/var/lib/rusty/dkim"\npublic_ips = ["%s"]\n\n' "$(manager_public_ip)"; blk_unseal; blk_blob; blk_store outbound; blk_metrics; }
conf_gc() { head_common; printf 'gc_grace_seconds = 604800\n\n[dns_check]\nenabled = true\n\n'; blk_unseal; blk_blob; blk_store gc; blk_metrics; }
conf_admin() {
  printf 'hostname = "%s"\n\n[admin]\nlisten = "0.0.0.0:8080"\npublic_origin = "https://%s"\npublic_ips = ["%s"]\nallowed_peers = ["127.0.0.1/32"]\nallowed_hosts = ["tasks.edge", "edge"]\n' "$MAIL_HOST" "$ADMIN_HOST" "$(manager_public_ip)"
  # shortcut: off until the admin image that knows the key is deployed (ADMIN_PROXY=1): an older admin refuses the unknown key.
  [ "$ADMIN_PROXY" != 1 ] || printf 'proxy_protocol = true\n'
  # The pilot is reached on its own port and the browser's Origin carries it: the admin web must know it (same release as proxy_protocol).
  [ "$ADMIN_PROXY" != 1 ] || [ "$EDGE" != prost-pilot ] || printf 'extra_origins = ["https://%s:%s"]\n' "$ADMIN_HOST" "$(edge_port 443)"
  printf '\n'; blk_store admin; blk_valkey admin
  printf '[sealing]\npublic_key = "%s"\n\n' "$(cat "$STATE_DIR/keys/seal.pub")"
  if [ -n "${METRICS_ALLOW:-}" ] || [ "$MONITORING" = 1 ]; then
    local c list=""
    for c in ${METRICS_ALLOW:-}; do list="${list:+$list, }\"$c\""; done
    printf '[metrics]\nlisten = "0.0.0.0:9100"\n'
    [ -z "$list" ] || printf 'allowed_peers = [%s]\n' "$list"
    # The Prometheus of the monitoring profile is found by its service name (no address is fixed anywhere).
    [ "$MONITORING" != 1 ] || printf 'allowed_hosts = ["tasks.prometheus"]\n'
    printf '\n'
    for c in smtp-in submission imap outbound gc; do printf '[[metrics.scrape]]\nservice = "%s"\naddress = "tasks.%s:9100"\n\n' "$c" "$c"; done
  fi
}
# The webmail (docs/design-webmail.md): behind the edge only, which ends TLS and sends the PROXY header. No sealing or unsealing key: it opens no sealed secret.
conf_webmail() {
  head_common
  printf '[webmail]\nlisten = "0.0.0.0:8090"\npublic_origin = "https://%s"\nallowed_peers = ["127.0.0.1/32"]\nallowed_hosts = ["tasks.edge", "edge"]\nproxy_protocol = true\n\n' "$WEBMAIL_HOST"
  blk_blob; blk_valkey webmail; blk_store webmail
}
conf_migrate() { head_common; printf 'gc_grace_seconds = 604800\n\n'; blk_blob; blk_store gc; printf 'migrator_user = "rusty_migrator"\nmigrator_password_file = "/run/secrets/pg_migrator"\n'; }

# The edge's configuration: the standalone template with the swarm's backends (every task of a service by `tasks.<service>`) and the certificates it has (self-signed here).
conf_haproxy() {
  local allow="$ADMIN_ALLOW" keep="#SELF " drop="#ACME" dir="${ACME_DIRECTORY:-https://acme-v02.api.letsencrypt.org/directory}"
  if [ "$TLS_MODE" = acme ]; then keep="#ACME"; drop="#SELF "; fi
  # The webmail's lines (marked `#WEBMAIL `) are in the file only with a WEBMAIL_HOST; the marker comes off first so that a line that is also `#ACME` or `#SELF` is then taken by those rules. Its certificate is one more `crt`.
  local wm_keep="/^#WEBMAIL /d" wm_crt=""
  if [ -n "$WEBMAIL_HOST" ]; then
    wm_keep="s|^#WEBMAIL ||"
    if [ "$TLS_MODE" = acme ]; then wm_crt=' crt "@certs/webmail.pem"'; else wm_crt=' crt /var/lib/haproxy/certs/webmail.pem'; fi
  fi
  sed -e "$wm_keep" -e "s|@WEBMAIL_CRT@|$wm_crt|g" -e "s|@WEBMAIL_HOST@|${WEBMAIL_HOST:-}|g" -e "/^${drop}/d" -e "s|^${keep}||" \
      -e "$([ "$ADMIN_PROXY" = 1 ] || echo '/server-template admin/s/ send-proxy-v2//')" \
      -e 's/\(server-template [a-z-]* [0-9]-[0-9]\) \([a-z-]*\):/\1 tasks.\2:/' \
      -e "s|@MAIL_HOST@|$MAIL_HOST|g" -e "s|@ADMIN_HOST@|$ADMIN_HOST|g" -e "s|@ADMIN_ALLOW@|$allow|g" -e "s|@CONTACT@|${CONTACT:-}|g" -e "s|@ACME_DIRECTORY@|$dir|g" \
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

# A CA of its own and a server certificate for one name, made once in DIR (ca.pem, server.key, server.crt). The default PostgreSQL and the key-value store both use it.
make_ca_signed() { # DIR CA-NAME HOST
  local dir="$1" ca="$2" host="$3" out
  mkdir -p "$dir"; chmod 0700 "$dir"
  if [ -s "$dir/ca.pem" ] && [ -s "$dir/server.crt" ] && [ -s "$dir/server.key" ]; then return 0; fi
  out="$(sw run --rm --cap-drop ALL --entrypoint sh "$POSTGRES_IMAGE" -c "set -e; cd /tmp
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-384 -nodes -days 3650 -subj '/CN=$ca' -keyout ca.key -out ca.pem 2>/dev/null
    openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-384 -nodes -subj '/CN=$host' -keyout server.key -out server.csr 2>/dev/null
    printf 'subjectAltName=DNS:$host\nextendedKeyUsage=serverAuth\n' > ext.cnf
    openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 3650 -extfile ext.cnf -out server.crt 2>/dev/null
    cat ca.pem; echo ----SPLIT----; cat server.key; echo ----SPLIT----; cat server.crt")"
  ( umask 077
    printf '%s\n' "$out" | awk 'BEGIN { n = 0 } /^----SPLIT----$/ { n++; next } n == 0 { print }' > "$dir/ca.pem"
    printf '%s\n' "$out" | awk 'BEGIN { n = 0 } /^----SPLIT----$/ { n++; next } n == 1 { print }' > "$dir/server.key"
    printf '%s\n' "$out" | awk 'BEGIN { n = 0 } /^----SPLIT----$/ { n++; next } n == 2 { print }' > "$dir/server.crt" )
  grep -q 'BEGIN CERTIFICATE' "$dir/ca.pem" && grep -q 'PRIVATE KEY' "$dir/server.key" && grep -q 'BEGIN CERTIFICATE' "$dir/server.crt" || die "could not make the certificates for $host"
}
make_db_tls() { make_ca_signed "$STATE_DIR/dbtls" "rusty db CA" db; }

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
  y_secret s3_access s3_access 10001   # the image runs as uid 10001 (rustfs)
  y_secret s3_secret s3_secret 10001
  printf '    networks:\n      - backend\n'
}

# ---- resource limits -------------------------------------------------------------------------------------------------------------------------------------------------------------------------
# limits.sh of the standalone stack divides 80 % of the memory and CPUs of the Ready nodes (read from the Manager) over the services by weight; its answer goes into `deploy.resources` and, for the
# inbound listeners, into `[limits] inflight_message_mib` (which must stay well under the container's memory). Run again after nodes join or leave.
LIMITS_FILE=""
gen_limits() {
  local mem cpus id st av n m profiles="" reps="" s info nodes=""
  mem=0; cpus=0
  while read -r id st av; do
    [ "$st" = Ready ] && [ "$av" = Active ] || continue
    read -r n m < <(sw node inspect "$id" --format '{{.Description.Resources.NanoCPUs}} {{.Description.Resources.MemoryBytes}}')
    cpus=$((cpus + n)); mem=$((mem + m))
    # One line per node: its memory in MiB, its role, its labels (to know which services may run there).
    info="$(sw node inspect "$id" --format '{{.Description.Resources.MemoryBytes}} {{.Spec.Role}} {{json .Spec.Labels}}')"
    nodes="$nodes$(( ${info%% *} / 1048576 )) ${info#* }"$'\n'
  done < <(sw node ls --format '{{.ID}} {{.Status}} {{.Availability}}')
  [ "$mem" -gt 0 ] || die "limits: no Ready and Active node"
  [ "$DB_IN_STACK" = 1 ] || profiles="nodb"
  [ "$S3_IN_STACK" = 0 ] || profiles="${profiles:+$profiles,}s3"
  [ "$FILTER" = 0 ] || profiles="${profiles:+$profiles,}filter"
  [ "$VALKEY" = 0 ] || profiles="${profiles:+$profiles,}valkey"
  for s in smtp-in submission imap outbound gc admin ${WEBMAIL_HOST:+webmail}; do [ "$(replicas_of "$s")" = 1 ] || reps="${reps:+$reps,}$s=$(replicas_of "$s")"; done
  LIMITS_FILE="$STATE_DIR/stack/limits.yaml"
  bash "$HERE/../standalone/limits.sh" --total-mem-mib "$((mem / 1048576))" --total-cpus "$(awk -v n="$cpus" 'BEGIN { printf "%.2f", n / 1000000000 }')" ${profiles:+--profiles "$profiles"} ${reps:+--replicas "$reps"} \
    --out "$LIMITS_FILE" > "$STATE_DIR/stack/limits.txt" || { cat "$STATE_DIR/stack/limits.txt" >&2; die "limits: the services do not fit in the nodes (see above)"; }
  cap_limits "$nodes"
}

# limits.sh divides the memory of ALL nodes as if it were one pool. A container lives on one node: its limit may not be more than 80 % of the smallest node it may be placed on (the rest is the host's),
# and it is reserved at a quarter of the limit so the scheduler counts it. The result is the table `limits.tsv` (service, MiB, CPUs, reserved MiB) that y_limits and inflight_of read.
cap_limits() { # NODES-TEXT
  local nodes="$1" name mem cpus cap pin min line nmem role labels lab out="$STATE_DIR/stack/limits.tsv"
  : > "$out"
  while read -r name mem cpus; do
    case "$name" in
      edge|edge-prost|certd) pin="manager" ;;
      outbound) pin='"outbound":"true"' ;;
      clamd|freshclam) pin='"av":"true"' ;;
      db) pin='"db":"true"' ;;
      s3) pin='"s3":"true"' ;;
      *) pin="" ;;
    esac
    min=""
    while read -r nmem role labels; do
      [ -n "$nmem" ] || continue
      if [ -n "$pin" ]; then
        if [ "$pin" = manager ]; then [ "$role" = manager ] || continue; else lab="$labels"; case "$lab" in *"$pin"*) ;; *) continue ;; esac; fi
      fi
      if [ -z "$min" ] || [ "$nmem" -lt "$min" ]; then min="$nmem"; fi
    done <<< "$nodes"
    [ -n "$min" ] || die "limits: no node can run $name (it needs a node $([ "$pin" = manager ] && echo 'that is a manager' || echo "labelled ${pin//\"/}"))"
    cap=$(( min * 80 / 100 ))
    [ "$mem" -le "$cap" ] || { echo "limits: $name is limited to $cap MiB, 80 % of the smallest node it can run on ($min MiB), not $mem MiB" >&2; mem="$cap"; }
    printf '%s %s %s %s\n' "$name" "$mem" "$cpus" "$(( mem / 4 ))" >> "$out"
  done < <(awk '/^  [a-z0-9-]+:$/ { n = $1; sub(/:$/, "", n) } /limits:/ { m = $0; sub(/.*memory: /, "", m); sub(/M.*/, "", m); c = $0; sub(/.*cpus: "/, "", c); sub(/".*/, "", c); print n, m, c }' "$LIMITS_FILE")
}
# The in-flight message budget (MiB) of an inbound listener: a third of its container's memory, at least 64 (empty when the service has no limit).
inflight_of() { awk -v n="$1" '$1 == n { v = int($2 / 3); if (v < 64) v = 64; print v }' "$STATE_DIR/stack/limits.tsv" 2>/dev/null; }
# The `resources` lines of a service (nothing when it has none).
y_limits() { awk -v n="$1" '$1 == n { printf "      resources:\n        limits: { cpus: \"%s\", memory: %sM }\n        reservations: { memory: %sM }\n", $3, $2, $4 }' "$STATE_DIR/stack/limits.tsv" 2>/dev/null; return 0; }


# ---- prepare secrets and prepare configs --------------------------------------------------------------------------------------------------------------------------------------------------
require_stack_inputs() {
  resolve_defaults
  [ -n "$MAIL_HOST" ] && [ -n "$ADMIN_HOST" ] && [ -n "$ADMIN_ALLOW" ] || die "the inventory needs MAIL_HOST, ADMIN_HOST and ADMIN_ALLOW"
  [ -n "$S3_ACCESS_KEY" ] && [ -r "$S3_SECRET_KEY_FILE" ] || die "S3_ENDPOINT is set but S3_ACCESS_KEY or S3_SECRET_KEY_FILE is not"
  case "$TLS_MODE" in
    self) ;;
    acme) [[ "${CONTACT:-}" =~ ^[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}$ ]] || die "TLS_MODE=acme needs CONTACT (an e-mail address: the certificate authority writes to it)" ;;
    *) die "TLS_MODE is self (self-signed certificates) or acme (certificates from Let's Encrypt)" ;;
  esac
  check_inputs
  check_edge_inputs
  monitoring_check_inputs
  make_passwords
}

cmd_prepare_secrets() {
  require_stack_inputs
  read_names || true
  local r d="$STATE_DIR/stack"
  mkdir -p "$d"; chmod 0700 "$d"
  make_sealing_key
  make_selfsigned mail "$MAIL_HOST"
  make_selfsigned admin "$ADMIN_HOST"
  [ -z "$WEBMAIL_HOST" ] || make_selfsigned webmail "$WEBMAIL_HOST"
  for r in "${SERVICE_ROLES[@]}"; do ensure_secret "pg_$r" "$(secret_file "$r")"; done
  ( umask 077; printf '%s' "$S3_ACCESS_KEY" > "$d/s3_access" )
  ensure_secret s3_access "$d/s3_access"
  ensure_secret s3_secret "$S3_SECRET_KEY_FILE"
  ensure_secret unseal_key "$STATE_DIR/keys/unseal.key"
  ensure_secret tls_cert "$STATE_DIR/tls/mail.crt"
  ensure_secret tls_key "$STATE_DIR/tls/mail.key"
  if [ "$TLS_MODE" = self ] && haproxy_is_edge; then
    ensure_secret edge_mail_pem "$STATE_DIR/tls/mail.pem"
    ensure_secret edge_admin_pem "$STATE_DIR/tls/admin.pem"
    [ -z "$WEBMAIL_HOST" ] || ensure_secret edge_webmail_pem "$STATE_DIR/tls/webmail.pem"
  fi
  edge_prepare_secrets "$d"
  if [ "$DB_IN_STACK" = 1 ]; then
    make_db_tls
    ensure_secret pg_postgres "$DB_ADMIN_PASSWORD_FILE"
    ensure_secret db_server_crt "$STATE_DIR/dbtls/server.crt"
    ensure_secret db_server_key "$STATE_DIR/dbtls/server.key"
  fi
  profile_secrets
  monitoring_secrets
  save_names
  echo "prepare secrets: done"
}

cmd_prepare_configs() {
  require_stack_inputs
  read_names || true
  [ -s "$STATE_DIR/keys/seal.pub" ] || die "no sealing key yet: run 'prepare secrets' first"
  local d="$STATE_DIR/stack/etc" s
  mkdir -p "$d"; chmod 0700 "$STATE_DIR/stack" "$d"
  gen_limits
  for s in smtp_in submission imap outbound gc admin ${WEBMAIL_HOST:+webmail} migrate; do
    ( umask 077; "conf_$s" | edge_hosts_filter > "$d/$s.toml" )
    ensure_config "$s" "$d/$s.toml"
  done
  edge_prepare_configs "$d"
  if [ "$DB_IN_STACK" = 1 ]; then make_db_tls; ( umask 077; conf_pg_hba > "$d/pg_hba.conf" ); ensure_config pg_hba "$d/pg_hba.conf"; fi
  if [ -n "$DB_CA" ]; then ensure_config db_ca "$DB_CA"; fi
  profile_configs
  monitoring_configs
  save_names
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

# The names the stack uses are those the last `prepare secrets` and `prepare configs` chose (a file in the state directory), not "the newest object": going back to an older content picks the old
# object again, and a name that no longer exists on the Manager is reported before anything is deployed.
names_file() { printf '%s/stack/names\n' "$STATE_DIR"; }
save_names() {
  local k f; f="$(names_file)"
  { for k in "${!SECRET_NAME[@]}"; do echo "S $k ${SECRET_NAME[$k]}"; done; for k in "${!CONFIG_NAME[@]}"; do echo "C $k ${CONFIG_NAME[$k]}"; done; } | sort > "$f.new" && mv "$f.new" "$f"
}
read_names() {
  local t k v
  [ -s "$(names_file)" ] || return 1
  while read -r t k v; do
    case "$t" in S) SECRET_NAME[$k]="$v" ;; C) CONFIG_NAME[$k]="$v" ;; esac
  done < "$(names_file)"
}
load_names() {
  local base need_s need_c
  read_names || die "nothing was prepared yet: run 'prepare secrets' and 'prepare configs'"
  need_s="${SERVICE_ROLES[*]/#/pg_} s3_access s3_secret unseal_key tls_cert tls_key"
  { [ "$TLS_MODE" != self ] || ! haproxy_is_edge; } || need_s="$need_s edge_mail_pem edge_admin_pem"
  { [ -z "$WEBMAIL_HOST" ] || [ "$TLS_MODE" != self ] || ! haproxy_is_edge; } || need_s="$need_s edge_webmail_pem"
  [ "$DB_IN_STACK" != 1 ] || need_s="$need_s pg_postgres db_server_crt db_server_key"
  need_c="smtp_in submission imap outbound gc admin ${WEBMAIL_HOST:+webmail} migrate"
  ! haproxy_is_edge || need_c="$need_c haproxy"
  local t k
  [ "$DB_IN_STACK" != 1 ] || need_c="$need_c pg_hba"
  [ -z "$DB_CA" ] || need_c="$need_c db_ca"
  while read -r t k; do case "$t" in S) need_s="$need_s $k" ;; C) need_c="$need_c $k" ;; esac; done < <(profile_names_needed; edge_names_needed; monitoring_names_needed)
  for base in $need_s; do
    [ -n "${SECRET_NAME[$base]:-}" ] && sw secret inspect "${SECRET_NAME[$base]}" >/dev/null 2>&1 || die "the secret $base is missing on the Manager: run 'prepare secrets'"
  done
  for base in $need_c; do
    [ -n "${CONFIG_NAME[$base]:-}" ] && sw config inspect "${CONFIG_NAME[$base]}" >/dev/null 2>&1 || die "the config $base is missing on the Manager: run 'prepare configs'"
  done
  # A name of a thing this inventory does not use must not reach the stack file.
  [ -n "$DB_CA" ] || unset 'CONFIG_NAME[db_ca]'
  if [ "$TLS_MODE" != self ] || ! haproxy_is_edge; then unset 'SECRET_NAME[edge_mail_pem]' 'SECRET_NAME[edge_admin_pem]'; fi
  if [ -z "$WEBMAIL_HOST" ] || [ "$TLS_MODE" != self ] || ! haproxy_is_edge; then unset 'SECRET_NAME[edge_webmail_pem]'; fi
  haproxy_is_edge || unset 'CONFIG_NAME[haproxy]'
  if [ "$DB_IN_STACK" != 1 ]; then unset 'SECRET_NAME[pg_postgres]' 'SECRET_NAME[db_server_crt]' 'SECRET_NAME[db_server_key]' 'CONFIG_NAME[pg_hba]'; fi
}

y_service_common() { # NAME IMAGE [READ-ONLY: true (default) or false, for an image that writes to its root file system]
  printf '  %s:
    image: %s
    read_only: %s
    cap_drop: [ALL]
    stop_grace_period: 45s
    volumes:
      - type: tmpfs
        target: /tmp
' "$1" "$2" "${3:-true}"
}
y_deploy() { # REPLICAS [CONSTRAINT] [ORDER] [SERVICE: its resource limits]
  printf '    deploy:\n      replicas: %s\n      restart_policy: { condition: any, delay: 5s }\n      update_config: { parallelism: 1, order: %s, delay: 10s, monitor: 20s, failure_action: rollback }\n      rollback_config: { parallelism: 1, order: %s }\n' "$1" "${3:-start-first}" "${3:-start-first}"
  [ -z "${2:-}" ] || printf '      placement:\n        constraints: ["%s"]\n' "$2"
  [ -z "${4:-}" ] || y_limits "$4"
}
y_hosts() { [ -z "$DB_HOSTADDR" ] || printf '    extra_hosts:\n      - "%s:%s"\n' "$DB_HOST" "$DB_HOSTADDR"; }

gen_rusty_service() { # NAME ROLE-OF-DB NETWORKS(space) [TLS] [UNSEAL] [CONSTRAINT]
  local name="$1" role="$2" nets="$3" tls="${4:-}" unseal="${5:-}" constraint="${6:-}" n key
  # The profiles' networks: smtp-in reaches the scanners; submission, imap and admin the key-value store.
  local kv=0
  case "$name" in
    smtp-in) [ "$FILTER" != 1 ] || nets="$nets filter" ;;
    submission|imap|admin|webmail) [ "$VALKEY" != 1 ] || { nets="$nets kv"; kv=1; } ;;
  esac
  y_service_common "$name" "$(rusty_image "$name")"
  y_deploy "$(replicas_of "$name")" "$constraint" start-first "$name"
  y_hosts
  printf '    secrets:\n'
  y_secret "pg_$role"
  if [ "$name" != admin ]; then y_secret s3_access; y_secret s3_secret; fi
  if [ "$unseal" = 1 ]; then y_secret unseal_key /etc/rusty-keys/unseal.key; fi
  if [ "$tls" = 1 ]; then y_secret tls_cert /etc/rusty-tls/cert.pem; y_secret tls_key /etc/rusty-tls/key.pem; fi
  if [ "$kv" = 1 ]; then y_secret "kv_$name"; fi
  printf '    configs:\n'
  key="${name//-/_}"
  y_config "$key" "/etc/rusty/$name.toml"
  if [ -n "${CONFIG_NAME[db_ca]:-}" ]; then y_config db_ca /etc/rusty/db-ca.pem; fi
  if [ "$kv" = 1 ]; then y_config kv_ca /etc/rusty/kv-ca.pem; fi
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
    [ "$FILTER" = 0 ] || gen_resolver_service
    if [ -z "$STACK_ONLY" ]; then
      # EDGE=prost: Prost is the edge (and prost-certd gets the certificates); EDGE=prost-pilot adds it beside HAProxy on other ports.
      [ "$EDGE" = haproxy ] || gen_prost_services
      if haproxy_is_edge; then
      # The edge: the one public entrance, on the Manager, its ports on the host (no routing mesh: the clients' addresses are kept).
      y_service_common edge "$(lock_ref 'haproxy:3.4.7-alpine3.24')"
      if [ "$TLS_MODE" = acme ]; then
        # The account key and the certificates the edge keeps live in a volume on the Manager; the runtime socket (cert sync reads the certificate from it) is a tmpfs.
        printf '      - edgedata:/var/lib/haproxy\n      - type: tmpfs\n        target: /run/ha\n        tmpfs: { mode: 1777 }\n'
      fi
      printf '    sysctls: { net.ipv4.ip_unprivileged_port_start: 0 }\n    ports:\n'
      local p
      local ports="25 80 110 143 443 465 587 993 995"
      for p in $ports; do printf '      - { target: %s, published: %s, protocol: tcp, mode: host }\n' "$p" "$p"; done
      y_deploy 1 "node.role == manager" stop-first edge
      if [ "$TLS_MODE" = self ]; then
        printf '    secrets:\n'
        y_secret edge_mail_pem /var/lib/haproxy/certs/mail.pem 99
        y_secret edge_admin_pem /var/lib/haproxy/certs/admin.pem 99
        [ -z "$WEBMAIL_HOST" ] || y_secret edge_webmail_pem /var/lib/haproxy/certs/webmail.pem 99
      fi
      printf '    configs:\n'
      y_config haproxy /usr/local/etc/haproxy/haproxy.cfg
      printf '    networks:\n      - app\n      - backend\n'
      fi
      gen_rusty_service smtp-in smtp_in "app backend metrics" 1 1
      gen_rusty_service submission submission "app backend metrics" 1 1
      gen_rusty_service imap imap "app backend metrics" 1 1
      gen_rusty_service outbound outbound "backend metrics" 0 1 'node.labels.outbound == true'
      gen_rusty_service gc gc "backend metrics" 0 1
      gen_rusty_service admin admin "app backend metrics" 0 0
      [ -z "$WEBMAIL_HOST" ] || gen_rusty_service webmail webmail "app backend metrics" 0 0
      [ "$FILTER" = 0 ] || gen_scanner_services
      [ "$VALKEY" = 0 ] || gen_valkey_service
      [ "$MONITORING" = 0 ] || gen_monitoring_services
    fi
    printf 'networks:\n  app: { driver: overlay, internal: true }\n  backend: { driver: overlay, attachable: true }\n  metrics: { driver: overlay, internal: true }\n'
    [ "$FILTER" = 0 ] || printf '  filter: { driver: overlay, internal: true }\n'
    [ "$VALKEY" = 0 ] || printf '  kv: { driver: overlay, internal: true }\n'
    if [ "$DB_IN_STACK" = 1 ] || [ "$S3_IN_STACK" = 1 ] || [ "$TLS_MODE" = acme ] || [ "$FILTER" = 1 ] || [ "$EDGE" != haproxy ] || [ "$MONITORING" = 1 ]; then
      printf 'volumes:\n'
      [ "$DB_IN_STACK" = 0 ] || printf '  pgdata:\n'
      [ "$S3_IN_STACK" = 0 ] || printf '  s3data:\n'
      { [ "$TLS_MODE" != acme ] || ! haproxy_is_edge; } || printf '  edgedata:\n'
      [ "$EDGE" = haproxy ] || edge_volumes
      [ "$MONITORING" = 0 ] || monitoring_volumes
      [ "$FILTER" = 0 ] || printf '  clamdb:\n'
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
  local jobs=(migrate) j state ndone failed rejected attempt
  [ "$S3_IN_STACK" = 0 ] || jobs+=(blobs-init)
  gen_migrate_stack > "$STATE_DIR/stack/migrate.yaml"
  # A job the swarm REJECTS (it could not even start: a network that was removed a moment ago is still being cleaned on the node, "Pool overlaps") is tried again, three times; a job that
  # starts and fails is not: that is the migration itself saying no.
  for attempt in 1 2 3; do
    sw stack rm "$MIGRATE_STACK" >/dev/null 2>&1 || true
    for _ in $(seq 1 30); do [ -z "$(sw service ls --filter "label=com.docker.stack.namespace=$MIGRATE_STACK" -q)" ] && break; sleep 1; done
    [ "$attempt" = 1 ] || sleep 8
    sw stack deploy --with-registry-auth -c - "$MIGRATE_STACK" < "$STATE_DIR/stack/migrate.yaml" >/dev/null
    for _ in $(seq 1 90); do
      ndone=0; failed=""; rejected=0
      for j in "${jobs[@]}"; do
        state="$(sw service ps "${MIGRATE_STACK}_$j" --format '{{.CurrentState}}' 2>/dev/null | head -1)"
        case "$state" in
          Complete*) ndone=$((ndone + 1)) ;;
          Failed*) failed="$failed $j" ;;
          Rejected*) failed="$failed $j"; rejected=1 ;;
        esac
      done
      if [ -n "$failed" ]; then
        for j in $failed; do
          echo "migrate: the job $j failed. The swarm says:" >&2
          sw service ps "${MIGRATE_STACK}_$j" --no-trunc --format '{{.CurrentState}}: {{.Error}}' 2>&1 | head -5 >&2
          echo "migrate: its log:" >&2
          sw service logs "${MIGRATE_STACK}_$j" 2>&1 | tail -20 >&2
        done
        if [ "$rejected" = 1 ] && [ "$attempt" -lt 3 ]; then echo "migrate: the swarm rejected the job: trying again" >&2; continue 2; fi
        sw stack rm "$MIGRATE_STACK" >/dev/null 2>&1 || true
        die "a job failed: nothing was deployed"
      fi
      if [ "$ndone" = "${#jobs[@]}" ]; then echo "migrate: the schema is up to date$([ "$S3_IN_STACK" = 1 ] && echo " and the bucket exists")"; sw stack rm "$MIGRATE_STACK" >/dev/null 2>&1 || true; return 0; fi
      sleep 2
    done
    for j in "${jobs[@]}"; do sw service logs "${MIGRATE_STACK}_$j" 2>&1 | tail -20 >&2 || true; done
    die "the jobs did not finish in time"
  done
}

cmd_deploy() {
  gen_stack > "$STATE_DIR/stack/stack.yaml"
  edge_prepare_volumes
  monitoring_prepare_volumes
  # --prune removes the services that are not in the file: the base file (the helpers only) must never prune the mail services of a system that is already running.
  local prune=(--prune); [ -z "$STACK_ONLY" ] || prune=()
  sw stack deploy --with-registry-auth "${prune[@]}" -c - "$STACK_NAME" < "$STATE_DIR/stack/stack.yaml"
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
  on "$(manager_name)" sh -c "if command -v nc >/dev/null 2>&1; then (sleep 2; echo QUIT) | nc -w 6 127.0.0.1 $1; elif command -v bash >/dev/null 2>&1; then bash -c 'exec 3<>/dev/tcp/127.0.0.1/$1; read -r -t 6 l <&3; printf \"%s\n\" \"\$l\"'; fi" 2>/dev/null | head -1 | tr -d '\r' || true   # a port that sends nothing is reported by the caller, not a reason to stop
}

cmd_verify_stack() {
  local bad=0 b p
  # The ports of the edge: the standard ones (HAProxy, or Prost when it is the edge) and, with the pilot, the ones of Prost beside it.
  for p in "" pilot; do
    [ "$p" = "" ] || [ "$EDGE" = prost-pilot ] || continue
    local o=0; [ "$p" = "" ] || o="$PROST_PILOT_OFFSET"
    b="$(banner $((25 + o)))"; case "$b" in "220 "*) echo "  ok    $((25 + o)): $b" ;; *) echo "  FAIL  $((25 + o)): no SMTP greeting through the edge ('$b')"; bad=1 ;; esac
    b="$(banner $((587 + o)))"; case "$b" in "220 "*) echo "  ok    $((587 + o)): $b" ;; *) echo "  FAIL  $((587 + o)): no SMTP greeting through the edge ('$b')"; bad=1 ;; esac
    b="$(banner $((143 + o)))"; case "$b" in "* OK"*) echo "  ok    $((143 + o)): $b" ;; *) echo "  FAIL  $((143 + o)): no IMAP greeting through the edge ('$b')"; bad=1 ;; esac
  done
  [ "$bad" = 0 ] || die "verify: a port does not answer as it should (./rusty-swarm logs edge, logs smtp-in ...)"
  echo "verify: the edge reaches the services and they accept it"
}

# ---- backup, restore, upgrade ----------------------------------------------------------------------------------------------------------------------------------------------------------------
# A dump of the database with the login that stays yours (pg_dump -Fc, from a container on a node: the same road the services take) and the sealing key. The S3 bucket is not in it: it is yours
# (or, with the default RustFS, the volume `s3data` on the node labelled s3=true): copy it the way you copy any bucket.
# shellcheck disable=SC2120 # called without a directory by upgrade
cmd_backup() { # [DIR] [--with-keys] [--with-blobs] [--keep N] | verify [FILE] | install-timer
  case "${1:-}" in
    verify) shift; cmd_backup_verify "$@"; return ;;
    install-timer) shift; cmd_backup_install_timer; return ;;
  esac
  local dir out="$STATE_DIR/backups" keys=0 blobs=0 keep="${BACKUP_KEEP:-0}"
  while [ $# -gt 0 ]; do case "$1" in --with-keys) keys=1; shift ;; --with-blobs) blobs=1; shift ;; --keep) keep="${2:?--keep needs a number}"; shift 2 ;; *) out="$1"; shift ;; esac; done
  [[ "$keep" =~ ^[0-9]+$ ]] || die "backup: --keep (BACKUP_KEEP) is a number of backups to keep (0: keep all)"
  dir="$out/$(date -u +%Y%m%dT%H%M%SZ)"
  [ -r "$DB_ADMIN_PASSWORD_FILE" ] || die "backup: DB_ADMIN_PASSWORD_FILE is not readable"
  ( umask 077; mkdir -p "$dir" )
  ( umask 077; pg_tool_as "pg_dump -Fc --no-owner" "$DB_ADMIN_USER" "$DB_ADMIN_PASSWORD_FILE" < /dev/null > "$dir/rusty.dump" ) || { rm -rf "$dir"; record_backup_run backup false - "pg_dump failed"; die "backup: pg_dump failed"; }
  [ -s "$dir/rusty.dump" ] || { rm -rf "$dir"; record_backup_run backup false - "the dump is empty"; die "backup: the dump is empty"; }
  # The sealing key opens the tenants' stored credentials that are IN the dump: it is kept apart from it unless you ask for both together.
  if [ "$keys" = 1 ] && [ -d "$STATE_DIR/keys" ]; then ( umask 077; cp -R "$STATE_DIR/keys" "$dir/keys" ); fi
  # The mail itself is in the S3 bucket. With RustFS inside the stack its volume is copied too (a copy of live files: complete for the mail that existed when the copy began; the database dump is the part
  # that is exactly consistent). An S3 of your own is yours to replicate.
  if [ "$blobs" = 1 ]; then
    [ "$S3_IN_STACK" = 1 ] || { rm -rf "$dir"; die "backup: --with-blobs copies the RustFS of the stack; with an S3 of your own replicate the bucket there"; }
    ( umask 077; sw run --rm --user 0 --cap-drop ALL --cap-add DAC_READ_SEARCH --security-opt no-new-privileges:true -v "${STACK_NAME}_s3data:/d:ro" --entrypoint tar "$(lock_ref 'curlimages/curl:8.15.0')" czf - -C /d . > "$dir/blobs.tar.gz" ) \
      || { rm -rf "$dir"; die "backup: the copy of the RustFS volume failed"; }
    [ -s "$dir/blobs.tar.gz" ] || { rm -rf "$dir"; die "backup: the copy of the RustFS volume is empty"; }
  fi
  record_backup_run backup true "$(wc -c < "$dir/rusty.dump")" "$([ "$blobs" = 1 ] && echo "with the mail of the RustFS volume" || echo "database only")"
  if [ "$blobs" = 1 ]; then echo "backup: $dir ($(wc -c < "$dir/rusty.dump") bytes of database,$(wc -c < "$dir/blobs.tar.gz") bytes of mail)."
  else echo "backup: $dir ($(wc -c < "$dir/rusty.dump") bytes of database). The S3 bucket is not in it (add --with-blobs for the RustFS of the stack)."; fi
  # Keep the newest N (only directories this tool made: a timestamp as the name).
  if [ "$keep" -gt 0 ]; then
    local old n=0 d
    # The directories of this tool, newest first (the names sort as the times do); the glob matches only a time stamp.
    for d in $(printf '%s\n' "$out"/[0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9]T[0-9][0-9][0-9][0-9][0-9][0-9]Z | sort -r); do
      [ -d "$d" ] || continue
      n=$((n + 1))
      [ "$n" -le "$keep" ] || { old="${d##*/}"; rm -rf "${out:?}/$old"; echo "backup: removed the old backup $old"; }
    done
  fi
  if [ "$keys" = 0 ]; then echo "backup: the sealing key ($STATE_DIR/keys/unseal.key) is NOT in it, on purpose: keep a copy of it somewhere else (without it the tenants' stored credentials cannot be read), or add --with-keys." >&2; fi
}

# What was done goes into the database (docs/design-backup-status.md), so that the Backups page of the admin web can show it; a row that cannot be written is a warning, never a failed backup.
record_backup_run() { # KIND (backup|verify) OK (true|false) BYTES|- DETAIL
  local kind="$1" ok="$2" bytes="$3" detail="$4" b="NULL"
  [ "$bytes" = - ] || b="$bytes"
  detail="${detail//\'/\'\'}"; detail="${detail:0:500}"
  printf "INSERT INTO backup_runs (kind, ok, bytes, detail) VALUES ('%s', %s, %s, '%s');\n" "$kind" "$ok" "$b" "$detail" \
    | pg_tool_as "psql -v ON_ERROR_STOP=1 -q -d" "$DB_ADMIN_USER" "$DB_ADMIN_PASSWORD_FILE" >/dev/null 2>&1 \
    || echo "backup: the run could not be recorded in the database (the Backups page will not show it)" >&2
}

# The drill, automatic: restore a dump into a scratch database next to the real one, read it, drop it. The real database is not touched. Needs a database user that may create databases (the superuser
# of the stack's own PostgreSQL does).
cmd_backup_verify() { # [DUMP-FILE]  (default: the newest backup)
  local f="${1:-}" scratch ts live out="" rc=0 q="psql -v ON_ERROR_STOP=1 -At -d"
  [ -n "$f" ] || f="$(ls -1d "$STATE_DIR"/backups/*/rusty.dump 2>/dev/null | sort | tail -1 || true)"
  [ -n "$f" ] && [ -s "$f" ] || die "backup verify: no dump (run 'backup' first, or name a rusty.dump)"
  [ -r "$DB_ADMIN_PASSWORD_FILE" ] || die "backup verify: DB_ADMIN_PASSWORD_FILE is not readable"
  ts="$(date -u +%Y%m%d%H%M%S)"; scratch="rusty_verify_$ts"
  echo "backup verify: restoring $f into the scratch database $scratch"
  ( DB_NAME=postgres; echo "CREATE DATABASE $scratch;" | pg_tool_as "$q" "$DB_ADMIN_USER" "$DB_ADMIN_PASSWORD_FILE" >/dev/null ) || die "backup verify: the scratch database could not be created (the user needs CREATEDB)"
  ( DB_NAME="$scratch"; pg_tool_as "pg_restore --no-owner -d" "$DB_ADMIN_USER" "$DB_ADMIN_PASSWORD_FILE" < "$f" ) || rc=1
  if [ "$rc" = 0 ]; then
    out="$( DB_NAME="$scratch"; printf '%s\n' "SELECT 'migrations=' || count(*) || ' newest=' || coalesce(max(version)::text, '-') FROM _sqlx_migrations;" "SELECT 'domains=' || count(*) FROM domains;" "SELECT 'accounts=' || count(*) FROM accounts;" "SELECT 'messages=' || count(*) FROM messages;" | pg_tool_as "$q" "$DB_ADMIN_USER" "$DB_ADMIN_PASSWORD_FILE" 2>&1 )" || rc=1
  fi
  ( DB_NAME=postgres; echo "DROP DATABASE IF EXISTS $scratch;" | pg_tool_as "$q" "$DB_ADMIN_USER" "$DB_ADMIN_PASSWORD_FILE" >/dev/null ) || echo "backup verify: the scratch database $scratch is left behind: drop it" >&2
  [ "$rc" = 0 ] || { printf '%s\n' "$out" >&2; record_backup_run verify false - "the dump could not be restored and read"; die "backup verify: the dump could not be restored and read: this backup is NOT good"; }
  live="$( printf '%s\n' "SELECT 'migrations=' || count(*) || ' newest=' || coalesce(max(version)::text, '-') FROM _sqlx_migrations;" | pg_tool_as "$q" "$DB_ADMIN_USER" "$DB_ADMIN_PASSWORD_FILE" 2>/dev/null || true )"
  printf 'backup verify: the dump holds    %s\n' "$(printf '%s' "$out" | tr '\n' ' ')"
  [ -z "$live" ] || printf 'backup verify: the live database  %s\n' "$live"
  record_backup_run verify true - "$(printf '%s' "$out" | tr '\n' ' ')"
  echo "backup verify: OK, the dump restores and reads"
}

# A systemd timer on the Manager: a backup every night (the newest BACKUP_KEEP, default 14, are kept), then the drill on it.
cmd_backup_install_timer() {
  local self env_file s="" unit=/etc/systemd/system/rusty-swarm-backup keep="${BACKUP_KEEP:-14}" blobs=""
  local runas=""
  [ "$(node_field "$(manager_name)" 2)" = local ] || die "backup install-timer: only for a Manager reached as 'local' (run this tool on it). From elsewhere: run '$0 backup --keep $keep' from cron"
  command -v systemctl >/dev/null 2>&1 || die "backup install-timer: no systemd here: run 'backup' from cron"
  [[ "$keep" =~ ^[1-9][0-9]*$ ]] || die "backup install-timer: BACKUP_KEEP is a number of backups (default 14)"
  [ "$S3_IN_STACK" != 1 ] || blobs=" --with-blobs"
  [ "$(id -u)" = 0 ] || s="sudo -n"
  # The backups belong to whoever runs this tool (the docker group), not to root: the unit runs as that user.
  if [ "$(id -u)" != 0 ]; then runas="User=$(id -un)"$'\n'; elif [ -n "${SUDO_USER:-}" ]; then runas="User=$SUDO_USER"$'\n'; fi
  self="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"; env_file="$(cd "$(dirname "$ENV_FILE")" && pwd)/$(basename "$ENV_FILE")"
  printf '[Unit]\nDescription=Rusty swarm: backup and drill\n\n[Service]\nType=oneshot\n%sEnvironment=HOME=%s\nExecStart=%s --env %s backup --keep %s%s\nExecStart=%s --env %s backup verify\n' "$runas" "$HOME" "$self" "$env_file" "$keep" "$blobs" "$self" "$env_file" | $s tee "$unit.service" >/dev/null
  printf '[Unit]\nDescription=Rusty swarm: backup every night\n\n[Timer]\nOnCalendar=*-*-* 02:30:00\nPersistent=true\nRandomizedDelaySec=900\n\n[Install]\nWantedBy=timers.target\n' | $s tee "$unit.timer" >/dev/null
  $s systemctl daemon-reload && $s systemctl enable --now rusty-swarm-backup.timer >/dev/null
  echo "backup install-timer: rusty-swarm-backup.timer makes a backup every night at 02:30 (keeping $keep) and checks it by restoring it into a scratch database"
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
  cmd_prune
  echo "upgrade: done"
}

# ---- certificates from Let's Encrypt (TLS_MODE=acme) -----------------------------------------------------------------------------------------------------------------------------------------
# The edge gets and renews the certificates of the two host names itself (HTTP-01 on port 80). The mail services carry STARTTLS themselves (ports 25, 587, 143 pass the edge untouched), so they need
# the certificate of MAIL_HOST as files: `certs sync` asks the edge for it (its runtime socket, from inside the edge container), and when it is new makes the next version of the secrets tls_cert and
# tls_key and rolls the services onto it. It also keeps both certificates in the edge's volume, so a restarted edge starts with them instead of ordering new ones (no rate-limit risk).
# Run it after the first deploy (init does, until the certificate is there) and then every few hours: `certs install-timer` makes a systemd timer on a Manager that runs this tool.
edge_container() { sw ps -q --filter "label=com.docker.swarm.service.name=${STACK_NAME}_edge" --filter status=running | head -1; }

cmd_certs_sync() {
  if [ "$TLS_MODE" != acme ]; then echo "certs sync: TLS_MODE is $TLS_MODE: nothing to do"; return 0; fi
  # Prost is the edge: its certificate comes from prost-certd, not from HAProxy's runtime socket.
  if [ "$EDGE" = prost ]; then cmd_certs_sync_prost; return; fi
  local c name host pem changed=0 dir="$STATE_DIR/tls" s3_changed=0
  read_names || true
  c="$(edge_container)"
  [ -n "$c" ] || { echo "certs sync: the edge is not running yet" >&2; return 1; }
  mkdir -p "$dir"; chmod 0700 "$dir"
  for name in mail admin ${WEBMAIL_HOST:+webmail}; do
    host="$MAIL_HOST"; [ "$name" = admin ] && host="$ADMIN_HOST"; [ "$name" = webmail ] && host="$WEBMAIL_HOST"
    pem="$(sw exec -i "$c" sh -c "echo 'dump ssl cert @certs/$name.pem' | socat -t5 - UNIX-CONNECT:/run/ha/admin.sock" 2>/dev/null || true)"
    if ! printf '%s\n' "$pem" | grep -q 'PRIVATE KEY' || ! printf '%s\n' "$pem" | grep -q 'BEGIN CERTIFICATE'; then
      echo "certs sync: the edge has no certificate for $host yet" >&2; return 1
    fi
    # The key and the chain apart; a combined file (the format HAProxy loads) is what the edge keeps.
    printf '%s\n' "$pem" | awk '/^-----BEGIN / { d = ($0 ~ /PRIVATE KEY/) ? "k" : "c" } d == "k" { print } /^-----END / { d = "" }' > "$dir/$name.acme.key.new"
    printf '%s\n' "$pem" | awk '/^-----BEGIN / { d = ($0 ~ /PRIVATE KEY/) ? "k" : "c" } d == "c" { print } /^-----END / { d = "" }' > "$dir/$name.acme.crt.new"
    cat "$dir/$name.acme.key.new" "$dir/$name.acme.crt.new" > "$dir/$name.acme.pem.new"
    sw exec -i -u 99 "$c" sh -c "umask 077; mkdir -p /var/lib/haproxy/certs && cat > /var/lib/haproxy/certs/$name.pem.new && mv /var/lib/haproxy/certs/$name.pem.new /var/lib/haproxy/certs/$name.pem" < "$dir/$name.acme.pem.new" || echo "certs sync: the edge could not keep the $name certificate" >&2
    # The certificates also go to the S3 of administration (CERT_S3=1), where the Prost pilot reads them; Prost loads a new one at its next start.
    if [ "$CERT_S3" = 1 ] && ! cmp -s "$dir/$name.acme.crt.new" "$dir/$name.s3.crt"; then
      if cert_s3_put "$name" "$dir/$name.acme.crt.new" "$dir/$name.acme.key.new"; then ( umask 077; cp "$dir/$name.acme.crt.new" "$dir/$name.s3.crt" ); s3_changed=1; fi
    fi
    if [ "$name" = mail ] && ! cmp -s "$dir/mail.acme.crt.new" "$dir/mail.crt"; then
      ( umask 077; cp "$dir/mail.acme.key.new" "$dir/mail.key"; cp "$dir/mail.acme.crt.new" "$dir/mail.crt" )
      ensure_secret tls_cert "$dir/mail.crt"
      ensure_secret tls_key "$dir/mail.key"
      save_names
      changed=1
    fi
    rm -f "$dir/$name.acme.key.new" "$dir/$name.acme.crt.new" "$dir/$name.acme.pem.new"
  done
  if [ "$s3_changed" = 1 ] && [ "$EDGE" = prost-pilot ]; then sw service update --force --detach=false "${STACK_NAME}_edge-prost" >/dev/null 2>&1 || echo "certs sync: the Prost pilot could not be restarted onto the new certificate" >&2; fi
  if [ "$changed" = 1 ]; then echo "certs sync: a new certificate for $MAIL_HOST; the mail services move onto it"; cmd_deploy; else echo "certs sync: the certificates are current"; fi
}

# After the first deploy the edge needs a minute to get its certificates: try until they are there.
wait_acme() {
  local _
  [ "$TLS_MODE" = acme ] || return 0
  for _ in $(seq 1 40); do
    if cmd_certs_sync 2>/dev/null; then return 0; fi
    sleep 15
  done
  echo "certs sync: the edge has no certificate after 10 minutes: are $MAIL_HOST and $ADMIN_HOST pointing at this server and port 80 open? (./rusty-swarm logs edge). The services run on a self-signed certificate until it comes; run 'certs sync' again later." >&2
  return 0
}

# A systemd timer on this machine (a Manager reached as `local`) that runs `certs sync` every six hours.
cmd_certs_install_timer() {
  local self env_file s="" unit=/etc/systemd/system/rusty-swarm-certs
  [ "$(node_field "$(manager_name)" 2)" = local ] || die "certs install-timer: only for a Manager reached as 'local' (run this tool on it). From elsewhere: run '$0 certs sync' from cron every few hours"
  command -v systemctl >/dev/null 2>&1 || die "certs install-timer: no systemd here: run 'certs sync' from cron"
  [ "$(id -u)" = 0 ] || s="sudo -n"
  self="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"; env_file="$(cd "$(dirname "$ENV_FILE")" && pwd)/$(basename "$ENV_FILE")"
  printf '[Unit]\nDescription=Rusty swarm: certificates from the edge to the mail services\n\n[Service]\nType=oneshot\nEnvironment=HOME=%s\nExecStart=%s --env %s certs sync\n' "$HOME" "$self" "$env_file" | $s tee "$unit.service" >/dev/null
  printf '[Unit]\nDescription=Rusty swarm: certificate sync every six hours\n\n[Timer]\nOnCalendar=*-*-* 00/6:17:00\nPersistent=true\nRandomizedDelaySec=600\n\n[Install]\nWantedBy=timers.target\n' | $s tee "$unit.timer" >/dev/null
  $s systemctl daemon-reload && $s systemctl enable --now rusty-swarm-certs.timer >/dev/null
  echo "certs install-timer: rusty-swarm-certs.timer runs 'certs sync' every six hours"
}

# ---- prune -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------
# Every change of a secret or a config makes a new object and the old one stays. This removes the ones nothing needs: not in the names this tool chose, not used by a running service, and not used by
# the PREVIOUS version of a service either (a rollback goes back to those). Old passwords do not linger on the Manager.
cmd_prune() {
  local used svc k n removed=0
  read_names || die "prune: nothing was prepared yet"
  used=" "
  for k in "${SECRET_NAME[@]}" "${CONFIG_NAME[@]}"; do used="$used$k "; done
  for svc in $(sw service ls --format '{{.Name}}'); do
    used="$used$(sw service inspect "$svc" --format '{{range .Spec.TaskTemplate.ContainerSpec.Secrets}}{{.SecretName}} {{end}}{{range .Spec.TaskTemplate.ContainerSpec.Configs}}{{.ConfigName}} {{end}}{{if .PreviousSpec}}{{range .PreviousSpec.TaskTemplate.ContainerSpec.Secrets}}{{.SecretName}} {{end}}{{range .PreviousSpec.TaskTemplate.ContainerSpec.Configs}}{{.ConfigName}} {{end}}{{end}}') "
  done
  for n in $(sw secret ls --filter label=rusty.base --format '{{.Name}}'); do
    case "$used" in *" $n "*) ;; *) if sw secret rm "$n" >/dev/null 2>&1; then removed=$((removed + 1)); fi ;; esac
  done
  for n in $(sw config ls --filter label=rusty.base --format '{{.Name}}'); do
    case "$used" in *" $n "*) ;; *) if sw config rm "$n" >/dev/null 2>&1; then removed=$((removed + 1)); fi ;; esac
  done
  echo "prune: removed $removed unused secret(s) and config(s)"
}
