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
  local n h e
  for n in mail:$2 admin:$3; do h="${n#*:}"; n="${n%%:*}"; for e in crt key pem; do echo x > "$s/tls/$n.$e"; done; printf '%s' "$h" > "$s/tls/$n.host"; done
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
! grep -q '^pop3' "$tmp/state-external/stack/etc/admin.toml" || fail "the admin web is told about POP3 in its file: the switch is in the database"
cfg="$tmp/state-external/stack/etc/haproxy.cfg"
grep -q '^frontend pop3_in' "$cfg" && grep -q '^frontend pop3s_in' "$cfg" || fail "the edge has no POP3 frontend"
grep -q 'bind :995 ssl crt /var/lib/haproxy/certs/mail.pem' "$cfg" || fail "port 995 does not end TLS with the mail certificate"
grep -q 'server-template imap 1-8 tasks.imap:1110 ' "$cfg" && grep -q 'server-template imap 1-8 tasks.imap:1995 ' "$cfg" || fail "the edge does not name the IMAP tasks for POP3"
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
for p in 25:10025 587:10587 143:10143 110:10110 465:10465 993:10993 995:10995 443:10443 80:10080; do grep -q "target: ${p#*:}, published: ${p%%:*}, protocol: tcp, mode: host" "$ps" || fail "prost: the edge does not publish ${p%%:*}"; done
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

scenario adminproxy "$EXT"$'\nEDGE=prost\nTLS_MODE=self\nADMIN_PROXY=1\nREPLICAS_DEFAULT=1' "edge admin" "certd db s3"
ap="$tmp/state-adminproxy/stack/etc"
grep -q '^proxy_protocol = true' "$ap/admin.toml" || fail "adminproxy: the admin web does not read the PROXY header"
grep -q '^send_proxy_v2 = true' <(sed -n '/upstreams.web]/,/endpoints/p' "$ap/prost-edge.toml") || fail "adminproxy: the edge does not send the PROXY header to the admin web"
prost_check adminproxy

scenario pilot "$EXT"$'\nEDGE=prost-pilot\nTLS_MODE=acme\nCERT_S3=1\nCERT_S3_BUCKET=certs\nREPLICAS_DEFAULT=1' "edge edge-prost smtp-in admin" "certd db s3"
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
refuse "an unknown edge" 'EDGE=nginx'
refuse "the pilot with ACME certificates and no bucket for them" $'EDGE=prost-pilot\nTLS_MODE=acme'
refuse "a pilot offset that is out of range" $'EDGE=prost-pilot\nPROST_PILOT_OFFSET=500'
refuse "the certificates in the bucket of the mail" $'EDGE=prost\nTLS_MODE=acme\nCERT_S3=1\nCERT_S3_BUCKET=rusty-mail'
refuse "a bad ADMIN_PROXY" 'ADMIN_PROXY=2'
refuse "a bad image" 'PROST_IMAGE="Prost Latest"'
echo "  ok    bad values are refused"
# For a look at what the tool rendered (SWARM_STACK_KEEP=DIR): the Prost edge configuration of the scenario "prost".
[ -z "${SWARM_STACK_KEEP:-}" ] || cp "$tmp/state-prost/stack/etc/prost-edge.toml" "$tmp/state-prost/stack/etc/prost-certd.toml" "$SWARM_STACK_KEEP/"
echo "swarm-stack OK"
