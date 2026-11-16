#!/usr/bin/env bash
# Prepares a standalone installation: secrets, the database CA, the sealing key, the configuration file of every service, and the images.
#
#   ./setup.sh --mail-host mail.example.org --admin-host admin.example.org --contact you@example.org \
#              --public-ip 203.0.113.5 --admin-allow 198.51.100.0/24 [--admin-allow 203.0.113.7] [--staging]
#   ./setup.sh --self-signed --mail-host mail.lan --admin-host admin.lan --public-ip 192.168.1.204 --admin-allow 192.168.1.0/24
#
# --self-signed: for a test installation the internet cannot reach (no ACME, no --contact): the certificates for the two host names are made here, self-signed,
# and valid for 825 days. Mail clients must be told to trust them (or to accept the warning). Without it the edge gets real certificates over ACME HTTP-01.
#
# Everything lands in the state directory (default ~/rusty-state, outside the repository) (secrets 0600 owned by the service uid). Run again to change the settings: secrets and keys that exist are kept, the
# configuration files are written again from the templates. Needs Docker with Compose (and BuildKit to build the images). Nothing here needs root
# except through Docker (the files are given to the service users by a throw-away container).
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
ROOT="$(cd ../.. && pwd)"
pin() { awk -v n="$1" '$1 == n { print $2 }' "$ROOT/deploy/images.lock"; }
PG_IMG="$(pin postgres:18)"; HA_IMG="$(pin haproxy:3.4.7-alpine3.24)"
[ -n "$PG_IMG" ] && [ -n "$HA_IMG" ] || { echo "setup: images are not pinned in deploy/images.lock" >&2; exit 1; }

STATE="${RUSTY_STATE:-$HOME/rusty-state}"; MAIL_HOST=""; ADMIN_HOST=""; CONTACT=""; PUBLIC_IP=""; STAGING=0; SELF=0; FILTER=0; BUILD=0; TAG=main; REGISTRY=ghcr.io/semi-system; ALLOW=()
while [ $# -gt 0 ]; do
  case "$1" in
    --mail-host) MAIL_HOST="${2:-}"; shift 2 ;;
    --admin-host) ADMIN_HOST="${2:-}"; shift 2 ;;
    --contact) CONTACT="${2:-}"; shift 2 ;;
    --public-ip) PUBLIC_IP="${2:-}"; shift 2 ;;
    --admin-allow) ALLOW+=("${2:-}"); shift 2 ;;
    --staging) STAGING=1; shift ;;
    --self-signed) SELF=1; shift ;;
    --filter) FILTER=1; shift ;;
    --build) BUILD=1; shift ;;
    --image-tag) TAG="${2:-}"; shift 2 ;;
    --registry) REGISTRY="${2:-}"; shift 2 ;;
    --state-dir) STATE="${2:-}"; shift 2 ;;
    *) echo "setup: unknown argument $1 (see the header of this script)" >&2; exit 2 ;;
  esac
done
die() { echo "setup: $*" >&2; exit 2; }
[[ "$TAG" =~ ^[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}$ ]] || die "--image-tag must be a registry tag (a release such as 1.2.3, sha-abcdef0, or main)"
[[ "$REGISTRY" =~ ^[a-z0-9.:/_-]+$ ]] || die "--registry must be a registry path such as ghcr.io/semi-system"
host_re='^[a-z0-9]([a-z0-9-]*[a-z0-9])?(\.[a-z0-9]([a-z0-9-]*[a-z0-9])?)+$'
[[ "$MAIL_HOST" =~ $host_re ]] || die "--mail-host must be a lower-case host name (mail.example.org)"
[[ "$ADMIN_HOST" =~ $host_re ]] || die "--admin-host must be a lower-case host name (admin.example.org)"
[ "$MAIL_HOST" != "$ADMIN_HOST" ] || die "the mail host and the admin host must differ"
[ "$SELF" = 1 ] || [[ "$CONTACT" =~ ^[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}$ ]] || die "--contact must be an email address (the certificate authority writes to it)"
[[ "$PUBLIC_IP" =~ ^[0-9]{1,3}(\.[0-9]{1,3}){3}$ ]] || die "--public-ip must be the IPv4 address mail leaves from"
[ ${#ALLOW[@]} -gt 0 ] || die "--admin-allow is required: the admin web answers only from the networks you list (an address or a CIDR block). It is the interim control until 2FA."
for a in "${ALLOW[@]}"; do [[ "$a" =~ ^[0-9]{1,3}(\.[0-9]{1,3}){3}(/[0-9]{1,2})?$ ]] || die "--admin-allow $a is not an IPv4 address or block"; [ "$a" != "0.0.0.0/0" ] || die "--admin-allow 0.0.0.0/0 would open the admin web to everyone"; done
if [ "$STAGING" = 1 ]; then ACME_DIR="https://acme-staging-v02.api.letsencrypt.org/directory"; else ACME_DIR="https://acme-v02.api.letsencrypt.org/directory"; fi

# Everything under ./state changes owner (the services run as other users), so after a first run the person running this script can no longer even look into
# some of it. Every look and every change inside ./state therefore goes through a throw-away root container (R: the haproxy image's shell; RP: the postgres
# image's, for openssl). That also makes this script safe to run again: what exists is found and kept (secrets, keys, certificates), never remade by mistake.
# The state lives OUTSIDE the repository (default ~/rusty-state): the image build reads the repository as its context, and a build cannot read directories that belong to
# the service users. An absolute path, written to .env for compose.
case "$STATE" in /*) ;; *) die "--state-dir must be an absolute path" ;; esac
mkdir -p "$STATE"
HOSTID="$(id -u):$(id -g)"; export HOSTID
R()  { docker run --rm --user 0 --entrypoint sh -e HOSTID -v "$STATE:/s" "$HA_IMG" -c "$1"; }
RP() { docker run --rm --user 0 --entrypoint sh -e HOSTID -v "$STATE:/s" "$PG_IMG" -c "$1"; }
R 'cd /s && mkdir -p secrets etc pgtls keys tls edge/acme edge/certs run && chown "$HOSTID" etc'
# Which kind of certificates the edge has decides which of the optional services (cert-sync, cert-keep) compose starts.
if [ "$SELF" = 1 ]; then profiles=""; else profiles="acme"; fi; compose_file=""; [ "$BUILD" = 1 ] && compose_file="compose.yaml:compose.build.yaml"
if [ "$FILTER" = 1 ]; then profiles="${profiles:+$profiles,}filter"; fi; printf 'RUSTY_STATE=%s
COMPOSE_PROFILES=%s
RUSTY_REGISTRY=%s
RUSTY_TAG=%s
' "$STATE" "$profiles" "$REGISTRY" "$TAG" > .env
[ -z "$compose_file" ] || printf 'COMPOSE_FILE=%s
' "$compose_file" >> .env
R 'cd /s/secrets && for s in pg_postgres pg_app pg_admin pg_migrator; do [ -s "$s" ] || (umask 077; head -c 32 /dev/urandom | od -An -tx1 | tr -d " \n" > "$s"); done'

# The database CA and the server certificate for the name `db` (clients verify it: sslmode verify-full). Made once.
if ! RP 'test -s /s/pgtls/server.key'; then
  RP 'set -e; umask 077; cd /s/pgtls
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-384 -nodes -days 3650 -subj "/CN=rusty db CA" -keyout ca.key -out ca.pem
    openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-384 -nodes -subj "/CN=db" -keyout server.key -out server.csr
    printf "subjectAltName=DNS:db\nextendedKeyUsage=serverAuth\n" > ext.cnf
    openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 3650 -extfile ext.cnf -out server.crt
    rm -f server.csr ext.cnf ca.srl; chmod 0644 ca.pem server.crt'
fi
R 'cp /s/pgtls/ca.pem /s/etc/db-ca.pem && chown "$HOSTID" /s/etc/db-ca.pem && chmod 0644 /s/etc/db-ca.pem'

render() { # template -> file: the values were validated above, none holds a character that matters to sed (| is never in them)
  local keep drop fkeep="#NOFILTER_"; [ "$FILTER" = 1 ] && fkeep="#FILTER "; if [ "$SELF" = 1 ]; then keep="#SELF "; drop="#ACME"; else keep="#ACME"; drop="#SELF "; fi
  sed -e "s|^${fkeep}||" -e "/^${drop}/d" -e "s|^${keep}||" -e "s|@MAIL_HOST@|$MAIL_HOST|g" -e "s|@ADMIN_HOST@|$ADMIN_HOST|g" -e "s|@CONTACT@|$CONTACT|g" -e "s|@PUBLIC_IP@|$PUBLIC_IP|g" \
      -e "s|@SEAL_PUBLIC@|$SEAL_PUBLIC|g" -e "s|@ACME_DIRECTORY@|$ACME_DIR|g" -e "s|@ADMIN_ALLOW@|${ALLOW[*]}|g" "templates/$1.in" > "$STATE/etc/$1"
}
# The sealing key pair: the private half stays in state/keys (mail uid only); the public half goes to both configurations.
mail_ctl() { docker run --rm --user 65532:65532 --entrypoint /usr/local/bin/rustyctl -v "$STATE/keys:/etc/rusty-keys" "$REGISTRY/rusty:$TAG" "$@"; }
if [ "${SKIP_BUILD:-0}" = 1 ]; then :   # SKIP_BUILD=1: the images are already here (a test installation that was built before)
elif [ "$BUILD" = 1 ]; then docker compose build >/dev/null   # development: build from this checkout (compose.build.yaml)
else docker compose pull --quiet; fi   # production: the images come from the registry, nothing is built on the server
#   # SKIP_BUILD=1: the images (rusty-mail:local for the one-shot jobs, rusty-<service>:local, rusty-admin:local) already exist
R 'chown 65532:65532 /s/keys /s/tls /s/secrets /s/secrets/* && chmod 0700 /s/keys /s/secrets && chmod 0600 /s/secrets/* && chmod 0755 /s/tls'
if ! R 'test -e /s/keys/unseal.key'; then mail_ctl seal keygen /etc/rusty-keys/unseal.key >/dev/null; fi
SEAL_PUBLIC="$(mail_ctl seal show /etc/rusty-keys/unseal.key | grep -o 'rusty-seal-v1:[0-9a-f]\{64\}' | head -1)"
[ -n "$SEAL_PUBLIC" ] || die "could not read the sealing public key"

for f in smtp-in.toml submission.toml imap.toml outbound.toml gc.toml ctl.toml admin.toml haproxy.cfg; do render $f; done
# Self-signed certificates for the two names (made once; a name change makes new ones): ECDSA P-256, with the names as subject alternative names. HAProxy loads
# a file holding the key then the certificate; the mail services read the certificate and the key of the mail name from state/tls.
if [ "$SELF" = 1 ]; then
  for pair in "mail:$MAIL_HOST" "admin:$ADMIN_HOST"; do
    n="${pair%%:*}"; h="${pair#*:}"
    if ! RP "test -s /s/edge/certs/$n.pem && openssl x509 -noout -checkhost $h < /s/edge/certs/$n.pem | grep -q 'does match'"; then
      RP "set -e; umask 077; cd /s/edge/certs
        openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 825 -subj \"/CN=$h\" -addext \"subjectAltName=DNS:$h\" -keyout $n.key -out $n.crt 2>/dev/null
        cat $n.key $n.crt > $n.pem; chmod 0600 $n.pem; chmod 0644 $n.crt"
    fi
  done
  R 'cp /s/edge/certs/mail.crt /s/tls/cert.pem && cp /s/edge/certs/mail.key /s/tls/key.pem'
fi
# The migration run needs the migrator role; the daemon's file never names it.
{ cat "$STATE/etc/gc.toml"; echo 'migrator_user = "rusty_migrator"'; echo 'migrator_password_file = "/run/secrets/pg_migrator"'; } > "$STATE/etc/migrate.toml"
chmod 0644 "$STATE"/etc/*

# The edge's files belong to its user (99), the postgres key to postgres (999).
R 'chown -R 99:99 /s/edge /s/run && chown 65532:65532 /s/tls/key.pem /s/tls/cert.pem 2>/dev/null; chmod 0600 /s/tls/key.pem 2>/dev/null; chmod 0700 /s/edge && chmod 0750 /s/run && chown -R 999:999 /s/pgtls && chmod 0600 /s/pgtls/server.key /s/pgtls/ca.key && chmod 0644 /s/pgtls/server.crt /s/pgtls/ca.pem'

cat <<DONE
setup: ready.
  start:            docker compose up -d       (profile from .env: $( [ "$SELF" = 1 ] && echo "self-signed certificates, no ACME" || echo "acme" ))
  first administrator: docker compose logs admin | tail -3     (the setup token), then https://$ADMIN_HOST/ from an allowed address
  DNS and mailbox:  docs/first-mail.md
$( [ "$SELF" = 1 ] && echo "  NOTE: the certificates are self-signed: mail clients must trust $MAIL_HOST (or accept the warning)." )
$( [ "$STAGING" = 1 ] && echo "  NOTE: the staging certificate authority is not trusted by mail clients; run setup.sh again without --staging when it works." )
DONE
