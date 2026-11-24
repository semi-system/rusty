#!/usr/bin/env bash
# The stack of deploy/swarm without a swarm: a fake `docker` answers what the tool asks of the Manager (two nodes, one small), the tool makes its secrets, configs and stack file for four inventories, and
# the result is checked: the stack file is valid Compose (when `docker compose` is here), every secret and config it names is declared, the services each inventory should have are there, a service pinned
# to the small node is limited to what that node can hold, the configuration keys match those of the standalone templates, and bad values in swarm.env are refused.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."
ROOT="$PWD"
TOOL="$ROOT/deploy/swarm/rusty-swarm"
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
REAL_DOCKER="$(command -v docker || true)"
fail() { echo "swarm-stack: $*" >&2; exit 1; }

# ---- the fake docker ------------------------------------------------------------------------------------------------------------------------------------------------------------------------
fake="$tmp/fake"; mkdir -p "$fake/secrets" "$fake/configs" "$tmp/bin"
cat > "$tmp/bin/docker" <<'EOS'
#!/usr/bin/env bash
F="$FAKE_DOCKER"
args="$*"
case "$args" in
  "node ls"*) printf 'n1 Ready Active\nn2 Ready Active\n' ;;
  "node inspect n1 --format {{.Description.Resources.NanoCPUs}}"*) echo "2000000000 4294967296" ;;
  "node inspect n2 --format {{.Description.Resources.NanoCPUs}}"*) echo "4000000000 17179869184" ;;
  "node inspect n1 --format {{.Description.Resources.MemoryBytes}}"*) echo '4294967296 manager {"outbound":"true","av":"true","db":"true","s3":"true"}' ;;
  "node inspect n2 --format {{.Description.Resources.MemoryBytes}}"*) echo '17179869184 worker {}' ;;
  "info --format {{.Name}}") echo n1 ;;
  "info --format {{.Swarm.NodeAddr}}") echo 10.0.0.1 ;;
  "info --format {{.Swarm.LocalNodeState}}") echo active ;;
  "network inspect"*) echo netid1 ;;
  "service inspect"*) echo "netid1 10.0.5.2/24" ;;
  "secret ls"*|"config ls"*)
    kind="${args%% *}"; base=""; sha=""
    for a in "$@"; do case "$a" in label=rusty.base=*) base="${a#label=rusty.base=}" ;; label=rusty.sha=*) sha="${a#label=rusty.sha=}" ;; esac; done
    for f in "$F/${kind}s"/*; do [ -e "$f" ] || continue; read -r b s < "$f"; [ "$b" = "$base" ] || continue; [ -z "$sha" ] || [ "$s" = "$sha" ] || continue; basename "$f"; done ;;
  "secret create"*|"config create"*)
    kind="${args%% *}"; base=""; sha=""; name=""
    for a in "$@"; do case "$a" in rusty.base=*) base="${a#rusty.base=}" ;; rusty.sha=*) sha="${a#rusty.sha=}" ;; esac; done
    for a in "$@"; do name="$a"; case "$a" in -|--label|rusty.*) ;; *) : ;; esac; done
    # the name is the argument before the final `-` (secrets and configs both take it from the content on stdin)
    set -- "$@"; n=$#; name="${@:$((n-1)):1}"
    cat > /dev/null; echo "$base $sha" > "$F/${kind}s/$name" ;;
  "secret inspect "*) [ -e "$F/secrets/${args#secret inspect }" ] ;;
  "config inspect "*) [ -e "$F/configs/${args#config inspect }" ] ;;
  *) : ;;
esac
EOS
chmod +x "$tmp/bin/docker"

# ---- the state the tool would have made on a real Manager (what needs Docker to be made) ---------------------------------------------------------------------------------------------------
prep_state() { # STATE_DIR MAIL_HOST ADMIN_HOST
  local s="$1"
  mkdir -p "$s/keys" "$s/tls" "$s/dbtls" "$s/kvtls" "$s/secrets"
  printf 'k' > "$s/keys/unseal.key"; echo "rusty-seal-v1:$(printf '0%.0s' $(seq 1 64))" > "$s/keys/seal.pub"
  printf 'k' > "$s/keys/webmail_totp.key"
  local n h e
  for n in mail:$2 admin:$3; do h="${n#*:}"; n="${n%%:*}"; for e in crt key pem; do echo x > "$s/tls/$n.$e"; done; printf '%s' "$h" > "$s/tls/$n.host"; done
  for e in crt key pem; do echo x > "$s/tls/webmail.$e"; done; printf "%s" webmail.example.org > "$s/tls/webmail.host"
  for d in dbtls kvtls; do echo x > "$s/$d/ca.pem"; echo x > "$s/$d/server.crt"; echo x > "$s/$d/server.key"; done
}

write_env() { # FILE STATE EXTRA...
  local f="$1" st="$2"; shift 2
  cat > "$f" <<ENV
NODES=("n1|local|manager|outbound=true,av=true,db=true,s3=true|203.0.113.10" "n2|local|worker||203.0.113.11")
STATE_DIR="$st"
MAIL_HOST=mail.example.org
ADMIN_HOST=admin.example.org
CONTACT=ops@example.org
ADMIN_ALLOW="203.0.113.5/32 198.51.100.0/24"
TLS_MODE=self
ENV
  printf '%s\n' "$@" >> "$f"
}
EXT=$'DB_HOST=db.example.org\nDB_ADMIN_USER=postgres\nDB_CA='"$tmp/ca.pem"$'\nS3_ENDPOINT=https://s3.example.org\nS3_ACCESS_KEY=AKIA123\nS3_SECRET_KEY_FILE='"$tmp/s3sk"
echo ca > "$tmp/ca.pem"; echo sk > "$tmp/s3sk"; echo pw > "$tmp/dbpw"
EXT="$EXT"$'\nDB_ADMIN_PASSWORD_FILE='"$tmp/dbpw"

run_tool() { # ENV ARGS...
  local e="$1"; shift
  FAKE_DOCKER="$fake" PATH="$tmp/bin:$PATH" bash "$TOOL" --env "$e" "$@"
}

# One scenario: prepare, print the stack, check it. $3 is a list of services that must be in it, $4 of services that must not.
scenario() { # NAME "ENV LINES" "must" "must not"
  local name="$1" extra="$2" must="$3" never="$4" st e out s
  st="$tmp/state-$name"; e="$tmp/$name.env"
  rm -rf "$fake/secrets"/* "$fake/configs"/*
  write_env "$e" "$st" ${extra:+"$extra"}
  prep_state "$st" mail.example.org admin.example.org
  run_tool "$e" prepare secrets >/dev/null 2>"$tmp/$name.err" || { cat "$tmp/$name.err" >&2; fail "$name: prepare secrets failed"; }
  run_tool "$e" prepare configs >/dev/null 2>"$tmp/$name.err" || { cat "$tmp/$name.err" >&2; fail "$name: prepare configs failed"; }
  out="$tmp/$name.stack.yaml"
  run_tool "$e" stack > "$out" 2>"$tmp/$name.err" || { cat "$tmp/$name.err" >&2; fail "$name: the stack file could not be made"; }
  for s in $must; do grep -q "^  $s:\$" "$out" || fail "$name: the service $s is not in the stack"; done
  for s in $never; do ! grep -q "^  $s:\$" "$out" || fail "$name: the service $s must not be in the stack"; done
  # every secret and config a service names is declared at the top level, and nothing declared has an empty name
  awk '/^secrets:$/ { t = "s"; next } /^configs:$/ { t = "c"; next } /^[a-z]/ { t = "" } t != "" && /^  [^ ].*: \{ external: true \}$/ { d[$1] = 1 } /^      - source: / { u[$3] = 1 } END { for (n in u) if (!((n ":") in d)) { print n; bad = 1 } exit bad }' "$out" > "$tmp/undeclared" || fail "$name: used but not declared: $(tr '\n' ' ' < "$tmp/undeclared")"
  if [ -n "$REAL_DOCKER" ] && "$REAL_DOCKER" compose version >/dev/null 2>&1; then
    "$REAL_DOCKER" compose -f "$out" config -q 2>"$tmp/$name.compose" || { cat "$tmp/$name.compose" >&2; fail "$name: docker compose does not accept the stack file"; }
  fi
  echo "  ok    $name"
}

echo "swarm-stack:"
scenario external "$EXT"$'\nREPLICAS_DEFAULT=1' "edge smtp-in submission imap outbound gc admin" "db s3 resolver rspamd clamd freshclam valkey"
grep -q 'edge_mail_pem' "$tmp/external.stack.yaml" || fail "self-signed: the edge has no certificate secret"
# memory: every service is capped by the small node it may run on (4096 MiB: 80 % is 3276), the reservation is a quarter, in-flight is a third
awk '/^  [a-z0-9-]+:$/ { n = $1 } /memory: [0-9]+M \}/ { m = $0; sub(/.*memory: /, "", m); sub(/M.*/, "", m); if (m + 0 > 3276) { print n " " m; bad = 1 } } END { exit bad }' "$tmp/external.stack.yaml" > "$tmp/over" || fail "a service is limited above 80 % of the small node: $(cat "$tmp/over")"
grep -q 'reservations: { memory: ' "$tmp/external.stack.yaml" || fail "no memory is reserved"
grep -q '^inflight_message_mib = ' "$tmp/state-external/stack/etc/smtp_in.toml" || fail "smtp-in has no in-flight budget"

scenario defaults $'TLS_MODE=acme\nS3_ACCESS_KEY=\nDB_HOST=\nS3_ENDPOINT=' "edge smtp-in submission imap outbound gc admin db s3" "resolver rspamd clamd freshclam valkey"
! grep -q 'edge_mail_pem' "$tmp/defaults.stack.yaml" || fail "acme: the edge must get its certificate itself, not from a secret"
# replicas: the Rusty services run two tasks (rolling updates never leave one without a task); the edge, the database, the object store and the admin console one
tasks_of() { awk -v s="$2" '$0 == "  " s ":" { on = 1; next } on && /^  [a-z0-9-]+:$/ { exit } on && /replicas:/ { gsub(/[^0-9]/, "", $2); print $2; exit }' "$1"; }
for s in smtp-in submission imap outbound gc; do [ "$(tasks_of "$tmp/defaults.stack.yaml" "$s")" = 2 ] || fail "$s does not run two tasks by default"; done
for s in edge db s3 admin; do [ "$(tasks_of "$tmp/defaults.stack.yaml" "$s")" = 1 ] || fail "$s must run one task by default"; done
grep -q 'edgedata:' "$tmp/defaults.stack.yaml" || fail "acme: no volume for the edge's account key and certificates"
grep -q '^  pgdata:' "$tmp/defaults.stack.yaml" && grep -q '^  s3data:' "$tmp/defaults.stack.yaml" || fail "the default PostgreSQL and S3 have no volumes"

scenario profiles "$EXT"$'\nFILTER=1\nVALKEY=1\nREPLICAS_DEFAULT=2\nMETRICS_ALLOW=10.1.0.0/16' "edge smtp-in submission imap outbound gc admin resolver rspamd clamd freshclam valkey" "db s3"
grep -q '^  filter: { driver: overlay, internal: true }' "$tmp/profiles.stack.yaml" && grep -q '^  kv: { driver: overlay, internal: true }' "$tmp/profiles.stack.yaml" || fail "the filter and kv networks are missing or not internal"
grep -q '^nameserver = "10.0.5.2:53"' "$tmp/state-profiles/stack/etc/smtp_in.toml" || fail "smtp-in does not name the resolver's address"
grep -q 'replicas: 2' "$tmp/profiles.stack.yaml" || fail "REPLICAS_DEFAULT=2 has no effect"

# POP3 is always open (whether it answers is a switch in the database, docs/design-protocols.md): the edge publishes 110 and 995, forwards them to the IMAP service and ends TLS on 995;
# the IMAP service listens; nothing in the environment turns it on or off.
grep -q 'published: 110' "$tmp/external.stack.yaml" && grep -q 'published: 995' "$tmp/external.stack.yaml" || fail "the edge does not publish 110 and 995"
grep -q '^listen_110 = "0.0.0.0:1110"' "$tmp/state-external/stack/etc/imap.toml" && grep -q '^listen_995 = "0.0.0.0:1995"' "$tmp/state-external/stack/etc/imap.toml" || fail "the IMAP service does not listen for POP3"
# ManageSieve (docs/design-sieve.md) is open the same way: published by the edge, passed through to the IMAP service on 4190 (its default: the stack names no port for it).
grep -q 'published: 4190' "$tmp/external.stack.yaml" || fail "the edge does not publish 4190"
! grep -q '^pop3' "$tmp/state-external/stack/etc/admin.toml" || fail "the admin web is told about POP3 in its file: the switch is in the database"
cfg="$tmp/state-external/stack/etc/haproxy.cfg"
grep -q '^frontend pop3_in' "$cfg" && grep -q '^frontend pop3s_in' "$cfg" || fail "the edge has no POP3 frontend"
grep -q 'bind :995 ssl crt /var/lib/haproxy/certs/mail.pem' "$cfg" || fail "port 995 does not end TLS with the mail certificate"
grep -q 'server-template imap 1-8 tasks.imap:1110 ' "$cfg" && grep -q 'server-template imap 1-8 tasks.imap:1995 ' "$cfg" || fail "the edge does not name the IMAP tasks for POP3"
grep -q '^frontend sieve_in' "$cfg" && grep -q 'server-template imap 1-8 tasks.imap:4190 ' "$cfg" || fail "the edge does not pass ManageSieve to the IMAP tasks"
# The rendered edge config keeps its shape: one be_admin backend, the admin frontend's default, no stray keyword.
for s in external defaults; do
  cfg="$tmp/state-$s/stack/etc/haproxy.cfg"
  [ "$(grep -c '^backend be_admin$' "$cfg")" = 1 ] || fail "$s: haproxy.cfg must have exactly one backend be_admin"
  grep -q '^    default_backend be_admin$' "$cfg" || fail "$s: the admin frontend lost its default_backend"
  ! grep -Eq 'default_[^b]' "$cfg" || fail "$s: haproxy.cfg has a stray default_ keyword"
  grep -q '^frontend pop3s_in' "$cfg" || fail "$s: the edge has no POP3 frontend"
done
echo "  ok    pop3 is always open at the edge and in the IMAP service; the edge config keeps its shape"

# ---- Prost as the edge (docs/design-prost-edge.md): EDGE=prost, the pilot beside HAProxy, and the three ways the certificates reach it ----------------------------------------------------------------------
# The rendered edge.toml is valid for the real Prost when its image is here (the check needs the files the paths name, so they are made).
# The rendered haproxy.cfg is valid for the real HAProxy of the lock when its image and openssl are here (the certificates it names are made for the check).
haproxy_check() { # NAME  (the haproxy.cfg of that scenario, TLS_MODE=self)
  local name="$1" img cfg d="$tmp/hapchk-$1" h
  img="$(awk '$1 == "haproxy:3.4.7-alpine3.24" { print $2 }' "$ROOT/deploy/images.lock")"
  cfg="$tmp/state-$name/stack/etc/haproxy.cfg"
  [ -n "$REAL_DOCKER" ] && [ -n "$img" ] && "$REAL_DOCKER" image inspect "$img" >/dev/null 2>&1 && command -v openssl >/dev/null 2>&1 || return 0
  mkdir -p "$d/certs"
  for h in admin mail webmail; do
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 30 -subj "/CN=$h.example.org" -keyout "$d/certs/$h.key" -out "$d/certs/$h.crt" 2>/dev/null
    cat "$d/certs/$h.key" "$d/certs/$h.crt" > "$d/certs/$h.pem"
  done
  cp "$cfg" "$d/haproxy.cfg"
  chmod -R a+rX "$d"
  "$REAL_DOCKER" run --rm -v "$d/haproxy.cfg:/usr/local/etc/haproxy/haproxy.cfg:ro" -v "$d/certs:/var/lib/haproxy/certs:ro" "$img" haproxy -c -f /usr/local/etc/haproxy/haproxy.cfg >"$tmp/hapchk-$name.out" 2>&1 \
    || { cat "$tmp/hapchk-$name.out" >&2; fail "$name: HAProxy does not accept the rendered configuration"; }
}
prost_check() { # NAME  (the edge.toml of that scenario)
  local name="$1" img="ghcr.io/semi-system/prost:latest" cfg n d
  d="$tmp/prostchk-$name"
  cfg="$tmp/state-$name/stack/etc/prost-edge.toml"
  [ -n "$REAL_DOCKER" ] && "$REAL_DOCKER" image inspect "$img" >/dev/null 2>&1 && command -v openssl >/dev/null 2>&1 || return 0
  mkdir -p "$d/certs/edge/current" "$d/secrets"
  # The state directory is private (0700) and the container runs as another user: the check reads a copy.
  cp "$cfg" "$d/edge.toml"; cfg="$d/edge.toml"
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 30 -subj "/CN=mail.example.org" -keyout "$d/certs/edge/current/privkey.pem" -out "$d/certs/edge/current/fullchain.pem" 2>/dev/null
  for n in edge_mail_crt edge_admin_crt; do cp "$d/certs/edge/current/fullchain.pem" "$d/secrets/$n"; done
  for n in edge_mail_key edge_admin_key; do cp "$d/certs/edge/current/privkey.pem" "$d/secrets/$n"; done
  chmod -R a+rX "$d"
  # S3 paths cannot be read without a bucket: that scenario is checked by its text only.
  ! grep -q '"s3://' "$cfg" || return 0
  "$REAL_DOCKER" run --rm -v "$cfg:/etc/prost/edge.toml:ro" -v "$d/certs:/certificates:ro" -v "$d/secrets:/run/secrets:ro" --entrypoint /usr/local/bin/prost "$img" check --config /etc/prost/edge.toml >"$tmp/prostchk-$name.out" 2>&1 \
    || { cat "$tmp/prostchk-$name.out" >&2; fail "$name: Prost does not accept the rendered edge configuration"; }
}
no_placeholders() { ! grep -n '@[A-Z_0-9]*@' "$1" >&2 || fail "a placeholder is left in $1"; }

scenario prost "$EXT"$'\nEDGE=prost\nTLS_MODE=acme\nREPLICAS_DEFAULT=1' "edge certd smtp-in submission imap outbound gc admin" "db s3"
ps="$tmp/prost.stack.yaml"; pe="$tmp/state-prost/stack/etc"
grep -q 'image: ghcr.io/semi-system/prost@sha256:' "$ps" || fail "prost: the edge does not use the pinned Prost image"
[ ! -e "$pe/haproxy.cfg" ] || fail "prost: HAProxy's configuration was made although Prost is the edge"
no_placeholders "$pe/prost-edge.toml"; no_placeholders "$pe/prost-certd.toml"
for p in 25:10025 587:10587 143:10143 110:10110 465:10465 993:10993 995:10995 4190:14190 443:10443 80:10080; do grep -q "target: ${p#*:}, published: ${p%%:*}, protocol: tcp, mode: host" "$ps" || fail "prost: the edge does not publish ${p%%:*}"; done
grep -q '^  prost_state:' "$ps" && grep -q 'uid=10001,gid=10001' "$ps" && grep -q 'prost_state:/var/lib/prost' "$ps" || fail "prost: the edge's state directory is not a volume owned by its user"
grep -q '"/usr/local/bin/prostctl", "status"' "$ps" || fail "prost: the health check does not name the binary by its path (the image has no PATH entry for it)"
grep -q '^  certd_state:' "$ps" && grep -q '^  certificates:' "$ps" && grep -q '^  prost_run:' "$ps" || fail "prost: the volumes of the edge and prost-certd are missing"
! grep -q '^  edgedata:' "$ps" || fail "prost: HAProxy's volume is in the stack"
grep -q 'certificate = "/certificates/edge/current/fullchain.pem"' "$pe/prost-edge.toml" || fail "prost: the edge does not read prost-certd's certificate"
grep -q '^address = "certd:8088"' "$pe/prost-edge.toml" || fail "prost: the edge does not route the ACME challenge to prost-certd"
grep -q 'path_prefix = "/.well-known/acme-challenge/"' "$pe/prost-edge.toml" || fail "prost: no route for the ACME challenge"
grep -q 'config = {require_allowlist=true, allow=\["203.0.113.5/32", "198.51.100.0/24"\]}' "$pe/prost-edge.toml" || fail "prost: the admin allow-list is not the inventory's"
grep -q '^domains = \["mail.example.org", "admin.example.org"\]' "$pe/prost-certd.toml" || fail "prost: prost-certd does not name both hosts"
grep -q '^contact = "mailto:ops@example.org"' "$pe/prost-certd.toml" || fail "prost: the ACME contact is not CONTACT"
grep -q '^address = "tasks.imap:1993"' "$pe/prost-edge.toml" && grep -q '^address = "tasks.submission:4465"' "$pe/prost-edge.toml" && grep -q '^address = "tasks.admin:8080"' "$pe/prost-edge.toml" || fail "prost: the edge does not name the services"
# The edge keeps its name `edge`: the services accept the PROXY header from it by that name, and the admin web too (it reads the client's address from the header).
grep -q '^trusted_proxy_hosts = \["tasks.edge", "edge"\]' "$pe/smtp_in.toml" || fail "prost: smtp-in does not trust the edge by name"
! grep -q '^proxy_protocol' "$pe/admin.toml" || fail "prost: the admin web reads the PROXY header although ADMIN_PROXY is off (an older admin image refuses the key)"
! grep -q "send_proxy_v2" <(sed -n '/upstreams.web]/,/endpoints/p' "$pe/prost-edge.toml") || fail "prost: the edge sends the PROXY header to the admin web with ADMIN_PROXY off"
prost_check prost

scenario prostself "$EXT"$'\nEDGE=prost\nTLS_MODE=self\nREPLICAS_DEFAULT=1' "edge smtp-in admin" "certd db s3"
no_placeholders "$tmp/state-prostself/stack/etc/prost-edge.toml"
grep -q 'certificate = "/run/secrets/edge_mail_crt"' "$tmp/state-prostself/stack/etc/prost-edge.toml" && grep -q 'target: edge_admin_key' "$tmp/prostself.stack.yaml" || fail "prostself: the self-signed certificates do not reach the edge as secrets"
! grep -q '^address = "certd:8088"' "$tmp/state-prostself/stack/etc/prost-edge.toml" || fail "prostself: an ACME route without prost-certd"
prost_check prostself
o="$(run_tool "$tmp/prostself.env" backup --keep abc 2>&1 || true)"; case "$o" in *"number of backups"*) ;; *) fail "backup: a bad --keep is not refused";; esac
o="$(run_tool "$tmp/prostself.env" backup verify 2>&1 || true)"; case "$o" in *"no dump"*|*"not readable"*) ;; *) fail "backup verify: a missing dump is not refused: $o";; esac
# The webmail behind Prost (docs/design-webmail.md): its host gets a certificate, a route and an upstream; without WEBMAIL_HOST none of it is in the file.
scenario prostwebmail "$EXT"$'\nEDGE=prost\nTLS_MODE=self\nWEBMAIL_HOST=webmail.example.org\nREPLICAS_DEFAULT=1' "edge webmail" "certd db s3"
pw="$tmp/state-prostwebmail/stack/etc/prost-edge.toml"
no_placeholders "$pw"
! grep -q '^#WEBMAIL\|@WEBMAIL' "$pw" || fail "prostwebmail: a marker or a placeholder is left"
grep -q '^server_names = \["webmail.example.org"\]' "$pw" && grep -q 'certificate = "/run/secrets/edge_webmail_crt"' "$pw" || fail "prostwebmail: the certificate of the webmail host is not given to the edge"
grep -q '^address = "tasks.webmail:8090"' "$pw" && grep -q '^upstream = "webmail"' "$pw" && grep -q 'hosts=\["admin.example.org", "mail.example.org", "webmail.example.org"\]' "$pw" || fail "prostwebmail: the route or the host list of the webmail is missing"
grep -q 'target: edge_webmail_key' "$tmp/prostwebmail.stack.yaml" || fail "prostwebmail: the key of the webmail host does not reach the edge as a secret"
grep -q '^send_proxy_v2 = true' "$pw" || fail "prostwebmail: the webmail is not told the client address"
prost_check prostwebmail
! grep -q 'webmail' "$tmp/state-prostself/stack/etc/prost-edge.toml" || fail "prostself: the webmail is in the edge without WEBMAIL_HOST"
scenario prostwebmailacme "$EXT"$'\nEDGE=prost\nTLS_MODE=acme\nWEBMAIL_HOST=webmail.example.org\nREPLICAS_DEFAULT=1' "edge webmail certd" "db s3"
grep -q '^domains = \["mail.example.org", "admin.example.org", "webmail.example.org"\]' "$tmp/state-prostwebmailacme/stack/etc/prost-certd.toml" || fail "prostwebmailacme: prost-certd does not ask for the webmail host"
grep -q 'server_names = \["webmail.example.org"\]' "$tmp/state-prostwebmailacme/stack/etc/prost-edge.toml" || fail "prostwebmailacme: the webmail host has no certificate entry"

printf "https://hooks.example.org/T000/secret-token
" > "$tmp/alert-url"
scenario monitoring "$EXT"$'
MONITORING=1
ALERT_URL_FILE='"$tmp/alert-url"$'
ALERT_KIND=slack
ALERT_REPEAT=2h
REPLICAS_DEFAULT=1' "prometheus alertmanager admin" "certd"
mp="$tmp/monitoring.stack.yaml"; me="$tmp/state-monitoring/stack/etc"
grep -q 'image: prom/prometheus@sha256:' "$mp" && grep -q 'image: prom/alertmanager@sha256:' "$mp" || fail "monitoring: the images are not the pinned ones"
grep -q '^  prom_data:' "$mp" && grep -q '^  am_data:' "$mp" || fail "monitoring: the volumes are missing"
grep -q 'api_url_file: /run/secrets/alert_url' "$me/alertmanager.yml" && grep -q 'repeat_interval: 2h' "$me/alertmanager.yml" || fail "monitoring: the Alertmanager configuration is not the inventory's"
! grep -rq "secret-token" "$mp" "$me" || fail "monitoring: the alert URL is in the stack file or a config (it belongs in the secret)"
grep -qxF 'allowed_hosts = ["tasks.prometheus"]' "$me/admin.toml" || fail "monitoring: the admin web does not let Prometheus scrape it"
grep -qF 'targets: ["tasks.admin:9100"]' "$me/prometheus.yml" || fail "monitoring: Prometheus does not scrape the admin web"
! grep -q "^  prometheus:" "$tmp/prost.stack.yaml" || fail "monitoring: a stack without MONITORING has Prometheus"

# Alerts by email: a smarthost with a login; the password is a secret, never in a config or the stack file.
printf 'smtp-pass-value\n' > "$tmp/smtp-pw"
scenario alertmail "$EXT"$'
MONITORING=1
ALERT_KIND=email
ALERT_EMAIL_TO=ops@example.org
ALERT_EMAIL_FROM=alerts@example.org
ALERT_SMARTHOST=mail.example.org:587
ALERT_SMTP_USER=alerts@example.org
ALERT_SMTP_PASSWORD_FILE='"$tmp/smtp-pw"$'
REPLICAS_DEFAULT=1' "prometheus alertmanager admin" "certd"
ap="$tmp/alertmail.stack.yaml"; ae="$tmp/state-alertmail/stack/etc"
grep -q 'smtp_smarthost: "mail.example.org:587"' "$ae/alertmanager.yml" && grep -q 'smtp_require_tls: true' "$ae/alertmanager.yml" && grep -q 'smtp_auth_password_file: /run/secrets/alert_smtp' "$ae/alertmanager.yml" && grep -q 'to: "ops@example.org"' "$ae/alertmanager.yml" || fail "alertmail: the Alertmanager configuration is not the inventory's"
! grep -q 'alert_url' "$ae/alertmanager.yml" "$ap" || fail "alertmail: a webhook secret in an email setup"
! grep -rq "smtp-pass-value" "$ap" "$ae" || fail "alertmail: the mail password is in the stack file or a config (it belongs in the secret)"
grep -q 'target: alert_smtp' "$ap" || fail "alertmail: the password secret does not reach Alertmanager"

scenario limits "$EXT"$'
EDGE=prost
TLS_MODE=self
PROST_MAX_PER_IP=500
PROST_CONN_RATE=200
PROST_CONN_BURST=1000
REPLICAS_DEFAULT=1' "edge" "certd db s3"
lq="$tmp/state-limits/stack/etc/prost-edge.toml"
no_placeholders "$lq"
grep -q '^max_connections_per_ip = 500$' "$lq" && grep -q 'connections_per_second=200, burst=1000,' "$lq" || fail "limits: the per-address limits of the edge are not the inventory's"
grep -q '^max_connections_per_ip = 50$' "$pe/prost-edge.toml" && grep -q 'connections_per_second=10, burst=100,' "$pe/prost-edge.toml" || fail "prost: the default per-address limits changed"
prost_check limits

haproxy_check external
scenario webmail "$EXT"$'\nWEBMAIL_HOST=webmail.example.org\nREPLICAS_DEFAULT=2' "edge smtp-in admin webmail" "certd"
wp="$tmp/webmail.stack.yaml"; we="$tmp/state-webmail/stack/etc"
grep -qx '  webmail:' "$wp" || fail "webmail: the service is missing"
grep -q 'public_origin = "https://webmail.example.org"' "$we/webmail.toml" && grep -q '^proxy_protocol = true' "$we/webmail.toml" && grep -qF 'allowed_hosts = ["tasks.edge", "edge"]' "$we/webmail.toml" || fail "webmail: its configuration is not the inventory's"
grep -q 'url = "postgres://rusty_webmail@' "$we/webmail.toml" || fail "webmail: it does not use its own database role"
! grep -qE 'unsealing|\[sealing\]|unseal_key' "$we/webmail.toml" || fail "webmail: it was handed a key"
wsvc="$(awk '/^  webmail:$/{f=1;next} /^  [a-z]/{f=0} f' "$wp")"
grep -q 'totp_key_file = "/etc/rusty-keys/totp.key"' "$we/webmail.toml" || fail "webmail: no key for the second factor in its configuration"
printf '%s\n' "$wsvc" | grep -q 'webmail_totp' || fail "webmail: the service does not mount the key of the second factor"
! printf '%s\n' "$wsvc" | grep -q 'unseal_key' || fail "webmail: the service mounts the unsealing key"
printf '%s\n' "$wsvc" | grep -q 'replicas: 1$' || fail "webmail: without Valkey the sessions are in the process: one task"
! grep -qx '  webmail:' "$tmp/prost.stack.yaml" || fail "a stack without WEBMAIL_HOST has a webmail"
echo "  ok    the webmail service (its own role, no key, one task without Valkey)"
haproxy_check webmail
# The edge entry of the webmail (HAProxy): its own certificate, its host name routed to it with no allow-list, the PROXY header, and nothing of it without WEBMAIL_HOST.
hc="$we/haproxy.cfg"
grep -q 'bind :443 ssl crt /var/lib/haproxy/certs/admin.pem crt /var/lib/haproxy/certs/mail.pem crt /var/lib/haproxy/certs/webmail.pem alpn' "$hc" || fail "webmail: the edge does not hold the webmail's certificate"
grep -qF 'acl webmail_host hdr(host),lower,field(1,:) -m str webmail.example.org' "$hc" && grep -q 'use_backend be_webmail if webmail_host' "$hc" && grep -q 'http-request allow if webmail_host' "$hc" || fail "webmail: the edge does not route its host name"
awk '/http-request allow if webmail_host/{a=NR} /http-request deny deny_status 403 unless admin_net/{d=NR} END{exit !(a && d && a < d)}' "$hc" || fail "webmail: its host name must be let through before the admin allow-list refuses"
grep -q 'server-template webmail 1-4 tasks.webmail:8090 .* send-proxy-v2' "$hc" || fail "webmail: the backend is not the task addresses with the PROXY header"
grep -q 'target: /var/lib/haproxy/certs/webmail.pem' "$wp" || fail "webmail: the self-signed certificate does not reach the edge as a secret"
! grep -q '@WEBMAIL\|^#WEBMAIL' "$hc" || fail "webmail: a placeholder or a marker is left in haproxy.cfg"
! grep -qi 'webmail' "$tmp/state-prost/stack/etc/haproxy.cfg" 2>/dev/null || fail "a configuration without WEBMAIL_HOST mentions the webmail"
scenario webmailacme "$EXT"$'\nWEBMAIL_HOST=webmail.example.org\nTLS_MODE=acme\nREPLICAS_DEFAULT=1' "edge webmail" "certd"
wa="$tmp/state-webmailacme/stack/etc/haproxy.cfg"
grep -q 'load crt "webmail.pem" acme letsencrypt domains "webmail.example.org"' "$wa" && grep -q 'crt "/var/lib/haproxy/certs/webmail.pem"\|crt "@certs/webmail.pem"\|webmail.pem" alpn' "$wa" || fail "webmailacme: the certificate of the webmail host is not ordered"
! grep -q '@WEBMAIL\|^#WEBMAIL' "$wa" || fail "webmailacme: a placeholder or a marker is left"
echo "  ok    the webmail's entry at the edge (HAProxy, self-signed and ACME)"

# The authoritative name server: a service of its own with a role of its own, port 53 published in host mode, the names handed to the admin web.
scenario dns "$EXT"$'\nDNS_NAME_SERVERS="ns1.example.org ns2.example.org"\nREPLICAS_DEFAULT=1' "dns admin" "certd"
dp="$tmp/dns.stack.yaml"; de="$tmp/state-dns/stack/etc"
dsvc="$(awk '/^  dns:$/{f=1;next} /^  [a-z]/{f=0} f' "$dp")"
[ -n "$dsvc" ] || fail "dns: the service is missing"
printf '%s\n' "$dsvc" | grep -q 'published: 53, protocol: udp, mode: host' && printf '%s\n' "$dsvc" | grep -q 'published: 53, protocol: tcp, mode: host' || fail "dns: port 53 is not published on UDP and TCP in host mode"
printf '%s\n' "$dsvc" | grep -q 'node.role == manager' || fail "dns: without a labelled node it does not stay on the Manager"
printf '%s\n' "$dsvc" | grep -q 'pg_dns' && ! printf '%s\n' "$dsvc" | grep -qE 's3_|unseal|webmail_totp|tls_' || fail "dns: it has a secret that is not its own role's password"
grep -q 'url = "postgres://rusty_dns@' "$de/dns.toml" && grep -q 'name_servers = \["ns1.example.org", "ns2.example.org"\]' "$de/dns.toml" && grep -q 'hostmaster = "hostmaster.example.org"' "$de/dns.toml" || fail "dns: its configuration is not the inventory's"
! grep -qE 'blob|unsealing|sealing|\[tls\]' "$de/dns.toml" || fail "dns: it was handed what a name server never uses"
grep -q 'name_servers = \["ns1.example.org", "ns2.example.org"\]' "$de/admin.toml" || fail "dns: the admin web is not told the names to set at the registrar"
! grep -q '^  dns:' "$tmp/webmail.stack.yaml" || fail "dns: a stack without DNS_NAME_SERVERS has the service"
echo "  ok    the authoritative name server (service, port 53, configuration)"

scenario adminproxy "$EXT"$'\nEDGE=prost\nTLS_MODE=self\nADMIN_PROXY=1\nREPLICAS_DEFAULT=1' "edge admin" "certd db s3"
ap="$tmp/state-adminproxy/stack/etc"
grep -q '^proxy_protocol = true' "$ap/admin.toml" || fail "adminproxy: the admin web does not read the PROXY header"
grep -q '^send_proxy_v2 = true' <(sed -n '/upstreams.web]/,/endpoints/p' "$ap/prost-edge.toml") || fail "adminproxy: the edge does not send the PROXY header to the admin web"
prost_check adminproxy

scenario pilot "$EXT"$'\nEDGE=prost-pilot\nTLS_MODE=acme\nCERT_S3=1\nCERT_S3_BUCKET=certs\nADMIN_PROXY=1\nREPLICAS_DEFAULT=1' "edge edge-prost smtp-in admin" "certd db s3"
grep -q '^extra_origins = \["https://admin.example.org:20443"\]' "$tmp/state-pilot/stack/etc/admin.toml" || fail "pilot: the admin web does not take the pilot's port as an origin (sign-in would be refused)"
pp="$tmp/pilot.stack.yaml"; pq="$tmp/state-pilot/stack/etc"
grep -q 'target: 10025, published: 20025, protocol: tcp, mode: host' "$pp" && grep -q 'target: 10993, published: 20993' "$pp" || fail "pilot: Prost is not on the offset ports"
grep -q 'target: 25, published: 25, protocol: tcp, mode: host' "$pp" || fail "pilot: HAProxy lost the real ports"
[ -s "$pq/haproxy.cfg" ] && [ -s "$pq/prost-edge.toml" ] || fail "pilot: both edge configurations are expected"
no_placeholders "$pq/prost-edge.toml"
grep -q 'certificate = "s3://certs/mail/fullchain.pem"' "$pq/prost-edge.toml" && grep -q 'private_key = "s3://certs/admin/privkey.pem"' "$pq/prost-edge.toml" || fail "pilot: the edge does not read the certificates from the bucket"
grep -q '^\[runtime.s3\]' "$pq/prost-edge.toml" && grep -q '^endpoint = "https://s3.example.org"' "$pq/prost-edge.toml" && grep -q '^force_path_style = true' "$pq/prost-edge.toml" || fail "pilot: [runtime.s3] is not the stack's S3"
! grep -q '^address = "certd:8088"' "$pq/prost-edge.toml" || fail "pilot: Prost must not answer ACME challenges beside HAProxy"
grep -q 'AWS_SHARED_CREDENTIALS_FILE: /run/secrets/edge_s3_credentials' "$pp" || fail "pilot: the S3 credentials are not given to the edge"
grep -q '^trusted_proxy_hosts = \["tasks.edge", "edge", "tasks.edge-prost", "edge-prost"\]' "$pq/smtp_in.toml" || fail "pilot: the services do not trust the Prost pilot by name"
grep -q '^allowed_hosts = \["tasks.edge", "edge", "tasks.edge-prost", "edge-prost"\]' "$pq/admin.toml" || fail "pilot: the admin web does not allow the Prost pilot"
! grep -q 'aws_access_key_id' "$pp" || fail "pilot: a key is in the stack file"
echo "  ok    Prost as the edge, as a pilot beside HAProxy, and its certificates (files, S3, secrets)"
echo "  ok    the webmail at the Prost edge (certificate, route, upstream)"
# Calendars and contacts (docs/design-caldav-carddav.md), DAV=1: a service of its own with a role of its own, no key and no mail, one task, and its routes at the edge (HAProxy and Prost).
scenario dav "$EXT"$'\nDAV=1\nREPLICAS_DEFAULT=2' "edge smtp-in admin dav" "certd"
dp="$tmp/dav.stack.yaml"; de="$tmp/state-dav/stack/etc"; dh="$de/haproxy.cfg"
grep -q 'url = "postgres://rusty_dav@' "$de/dav.toml" && grep -q '^proxy_protocol = true' "$de/dav.toml" && grep -q '^listen = "0.0.0.0:8091"' "$de/dav.toml" || fail "dav: its configuration is not the service's own"
grep -qF 'allowed_hosts = ["tasks.edge", "edge"]' "$de/dav.toml" || fail "dav: it does not trust the edge by name"
! grep -qE 'unsealing|\[sealing\]|unseal_key|blob_dir|blob_s3|\[valkey\]' "$de/dav.toml" || fail "dav: it was handed a key, a blob store or a key-value store"
dsvc="$(awk '/^  dav:$/{f=1;next} /^  [a-z]/{f=0} f' "$dp")"
printf '%s\n' "$dsvc" | grep -q 'pg_dav' || fail "dav: the service does not mount the password of its role"
! printf '%s\n' "$dsvc" | grep -qE 's3_access|s3_secret|unseal_key|webmail_totp' || fail "dav: the service mounts a secret it has no use for"
printf '%s\n' "$dsvc" | grep -q 'replicas: 1$' || fail "dav: one task (the sign-in counters are in its memory)"
printf '%s\n' "$dsvc" | grep -q 'rusty-dav:' || fail "dav: the image is not rusty-dav"
grep -q 'acl dav_path path -m str /.well-known/caldav' "$dh" && grep -q 'use_backend be_dav if mail_host dav_path' "$dh" || fail "dav: the edge does not send /dav/ and the well-known paths to it"
awk '/http-request allow if mail_host dav_path/{a=NR} /http-request deny deny_status 421 unless admin_host/{d=NR} END{exit !(a && d && a < d)}' "$dh" || fail "dav: its allowance comes after the refusals"
grep -q 'server-template dav 1-4 tasks.dav:8091 .* send-proxy-v2' "$dh" || fail "dav: the backend is not the task addresses with the PROXY header"
! grep -q '^#DAV\|@DAV' "$dh" || fail "dav: a marker is left in haproxy.cfg"
! grep -q 'dav' "$tmp/state-external/stack/etc/haproxy.cfg" || fail "a stack without DAV=1 has a DAV route"
! grep -qx '  dav:' "$tmp/external.stack.yaml" || fail "a stack without DAV=1 has a DAV service"
scenario davprost "$EXT"$'\nEDGE=prost\nTLS_MODE=self\nDAV=1\nREPLICAS_DEFAULT=1' "edge dav" "certd db s3"
dq="$tmp/state-davprost/stack/etc/prost-edge.toml"
no_placeholders "$dq"
! grep -q '^#DAV' "$dq" || fail "davprost: a marker is left"
grep -q '^\[upstreams.dav\]' "$dq" && grep -q '^address = "tasks.dav:8091"' "$dq" && grep -q '^upstream = "dav"' "$dq" || fail "davprost: the upstream or a route of the DAV service is missing"
[ "$(grep -c '^upstream = "dav"' "$dq")" = 3 ] || fail "davprost: /dav and the two well-known paths are routes of the mail host"
grep -q '^path_prefix = "/.well-known/carddav"' "$dq" || fail "davprost: no route for the well-known address of CardDAV"
prost_check davprost
! grep -q 'dav' "$tmp/state-prostself/stack/etc/prost-edge.toml" || fail "prostself: a DAV upstream without DAV=1"
echo "  ok    the calendar and contacts service and its routes (HAProxy and Prost)"
# The volume of a HAProxy edge with ACME: HAProxy does not make `acme/` (a new installation then restarted the edge for ever, "cannot create the file account.key"), so the deploy makes it, owned by the user 99.
vlog="$tmp/volumes.log"; : > "$vlog"
(
  # shellcheck disable=SC2034  # read by the function evaluated below
  STACK_NAME=rusty EDGE=haproxy TLS_MODE=acme
  sw() { printf '%s\n' "$*" >> "$vlog"; }
  lock_ref() { echo curlimg; }
  die() { echo "$*" >&2; exit 1; }
  haproxy_is_edge() { [ "$EDGE" != prost ]; }
  prost_has_certd() { return 1; }
  eval "$(sed -n '/^edge_prepare_volumes() {$/,/^}$/p' "$ROOT/deploy/swarm/edge-prost.sh")"
  edge_prepare_volumes
  TLS_MODE=self
  edge_prepare_volumes
) || fail "edge_prepare_volumes failed"
grep -q 'volume create --label com.docker.stack.namespace=rusty rusty_edgedata' "$vlog" && grep -q 'mkdir -p /v /v/acme; chown 99:99 /v /v/acme' "$vlog" || fail "the volume of the HAProxy edge is not given its acme directory and its owner"
[ "$(grep -c 'volume create' "$vlog")" = 1 ] || fail "a volume was made for a stack that needs none (HAProxy with self-signed certificates)"
echo "  ok    the volume of a HAProxy edge with ACME"

# ---- the configuration keys match those of the standalone templates ----------------------------------------------------------------------------------------------------------------------------
# section.key of a TOML text; a template's profile markers are switched on, its other comments dropped
keys_of() { sed -e 's/^#\(FILTER\|VALKEY\|METRICS\) //' -e '/^#/d' | awk '/^\[\[?[a-z0-9_.]+\]\]?$/ { s = $0; gsub(/[][]/, "", s); next } /^[a-z_]+ *=/ { k = $1; print s "." k }' | sort -u; }
tpl_ok=1
for svc in smtp-in submission imap outbound gc admin; do
  key="${svc//-/_}"
  keys_of < "deploy/standalone/templates/$svc.toml.in" > "$tmp/std.keys"
  keys_of < "$tmp/state-profiles/stack/etc/$key.toml" > "$tmp/swarm.keys"
  # what the swarm form does differently on purpose: the edge and the admin web by name, a bucket instead of a directory, a memory budget
  case "$svc" in admin) allowed_extra='^(admin.allowed_hosts)$' ;; *) allowed_extra='^(smtp.trusted_proxy_hosts|imap.trusted_proxy_hosts|blob_s3\..*|limits.inflight_message_mib)$' ;; esac
  # admin.proxy_protocol: the swarm form leaves it off until ADMIN_PROXY=1 (adminproxy scenario)
  missing="$(comm -23 "$tmp/std.keys" "$tmp/swarm.keys" | { grep -vx 'admin.proxy_protocol' || true; } | tr '\n' ' ')"
  extra="$(comm -13 "$tmp/std.keys" "$tmp/swarm.keys" | grep -Ev "$allowed_extra" | tr '\n' ' ' || true)"
  [ -z "$missing" ] || { echo "swarm-stack: $svc: keys of the standalone template that the swarm form lacks: $missing" >&2; tpl_ok=0; }
  [ -z "$extra" ] || { echo "swarm-stack: $svc: keys of the swarm form that the standalone template lacks: $extra" >&2; tpl_ok=0; }
done
[ "$tpl_ok" = 1 ] || fail "the swarm form and the standalone templates have drifted apart (see above)"
echo "  ok    configuration keys match the standalone templates"

# ---- bad values in swarm.env are refused before anything is generated -----------------------------------------------------------------------------------------------------------------------------
refuse() { # NAME "ENV LINE"
  local e="$tmp/bad.env"
  rm -rf "$fake/secrets"/* "$fake/configs"/*
  write_env "$e" "$tmp/state-bad" "$EXT" "$2"
  prep_state "$tmp/state-bad" mail.example.org admin.example.org
  if run_tool "$e" prepare configs >/dev/null 2>&1; then fail "a bad value was accepted: $1"; fi
}
refuse "a quote in MAIL_HOST" 'MAIL_HOST=mail".example.org'
refuse "everyone in ADMIN_ALLOW" 'ADMIN_ALLOW="0.0.0.0/0"'
refuse "a bad block in ADMIN_ALLOW" 'ADMIN_ALLOW="10.0.0.1/33"'
refuse "a bad bucket" 'S3_BUCKET=Bad_Bucket'
refuse "a bad endpoint" 'S3_ENDPOINT="https://s3.example.org/x y"'
refuse "a bad port" 'DB_PORT=99999'
refuse "a bad database name" 'DB_NAME=Rusty-DB'
refuse "two admin tasks without the shared session store" 'REPLICAS_ADMIN=2'
refuse "no tasks" 'REPLICAS_DEFAULT=0'
refuse "a name server that is not a host name" 'DNS_NAME_SERVERS="ns1.example.org bad name!"'
refuse "a name server with no domain to take the hostmaster from" 'DNS_NAME_SERVERS="ns1"'
refuse "an unknown edge" 'EDGE=nginx'
refuse "the pilot with ACME certificates and no bucket for them" $'EDGE=prost-pilot\nTLS_MODE=acme'
refuse "a pilot offset that is out of range" $'EDGE=prost-pilot\nPROST_PILOT_OFFSET=500'
refuse "the certificates in the bucket of the mail" $'EDGE=prost\nTLS_MODE=acme\nCERT_S3=1\nCERT_S3_BUCKET=rusty-mail'
refuse "a bad ADMIN_PROXY" 'ADMIN_PROXY=2'
refuse "monitoring without the URL file" 'MONITORING=1'
refuse "email alerts without a recipient" 'MONITORING=1
ALERT_KIND=email
ALERT_EMAIL_FROM=a@example.org
ALERT_SMARTHOST=mail.example.org:587'
refuse "email alerts with a login and no password file" 'MONITORING=1
ALERT_KIND=email
ALERT_EMAIL_TO=o@example.org
ALERT_EMAIL_FROM=a@example.org
ALERT_SMARTHOST=mail.example.org:587
ALERT_SMTP_USER=a'
refuse "monitoring with a URL file that is not a URL" 'MONITORING=1
ALERT_URL_FILE=/etc/hostname'
refuse "a bad alert kind" 'ALERT_KIND=sms'
refuse "a per-address limit of zero" 'PROST_MAX_PER_IP=0'
refuse "the webmail on the admin host" 'WEBMAIL_HOST=admin.example.org'
refuse "the webmail on something that is not a host name" 'WEBMAIL_HOST="not a host"'
refuse "a bad image" 'PROST_IMAGE="Prost Latest"'
echo "  ok    bad values are refused"
# For a look at what the tool rendered (SWARM_STACK_KEEP=DIR): the Prost edge configuration of the scenario "prost".
[ -z "${SWARM_STACK_KEEP:-}" ] || cp "$tmp/state-prost/stack/etc/prost-edge.toml" "$tmp/state-prost/stack/etc/prost-certd.toml" "$SWARM_STACK_KEEP/"
echo "swarm-stack OK"
