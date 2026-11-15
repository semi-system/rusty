#!/usr/bin/env bash
# Prepares a standalone installation: secrets, the database CA, the sealing key, the three configuration files, and the images.
#
#   ./setup.sh --mail-host mail.example.org --admin-host admin.example.org --contact you@example.org \
#              --public-ip 203.0.113.5 --admin-allow 198.51.100.0/24 [--admin-allow 203.0.113.7] [--staging]
#
# Everything lands in ./state (secrets 0600 owned by the service uid). Run again to change the settings: secrets and keys that exist are kept, the
# configuration files are written again from the templates. Needs Docker with Compose (and BuildKit to build the images). Nothing here needs root
# except through Docker (the files are given to the service users by a throw-away container).
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
ROOT="$(cd ../.. && pwd)"
pin() { awk -v n="$1" '$1 == n { print $2 }' "$ROOT/deploy/images.lock"; }
PG_IMG="$(pin postgres:18)"; HA_IMG="$(pin haproxy:3.4.7-alpine3.24)"
[ -n "$PG_IMG" ] && [ -n "$HA_IMG" ] || { echo "setup: images are not pinned in deploy/images.lock" >&2; exit 1; }

MAIL_HOST=""; ADMIN_HOST=""; CONTACT=""; PUBLIC_IP=""; STAGING=0; ALLOW=()
while [ $# -gt 0 ]; do
  case "$1" in
    --mail-host) MAIL_HOST="${2:-}"; shift 2 ;;
    --admin-host) ADMIN_HOST="${2:-}"; shift 2 ;;
    --contact) CONTACT="${2:-}"; shift 2 ;;
    --public-ip) PUBLIC_IP="${2:-}"; shift 2 ;;
    --admin-allow) ALLOW+=("${2:-}"); shift 2 ;;
    --staging) STAGING=1; shift ;;
    *) echo "setup: unknown argument $1 (see the header of this script)" >&2; exit 2 ;;
  esac
done
die() { echo "setup: $*" >&2; exit 2; }
host_re='^[a-z0-9]([a-z0-9-]*[a-z0-9])?(\.[a-z0-9]([a-z0-9-]*[a-z0-9])?)+$'
[[ "$MAIL_HOST" =~ $host_re ]] || die "--mail-host must be a lower-case host name (mail.example.org)"
[[ "$ADMIN_HOST" =~ $host_re ]] || die "--admin-host must be a lower-case host name (admin.example.org)"
[ "$MAIL_HOST" != "$ADMIN_HOST" ] || die "the mail host and the admin host must differ"
[[ "$CONTACT" =~ ^[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}$ ]] || die "--contact must be an email address (the certificate authority writes to it)"
[[ "$PUBLIC_IP" =~ ^[0-9]{1,3}(\.[0-9]{1,3}){3}$ ]] || die "--public-ip must be the IPv4 address mail leaves from"
[ ${#ALLOW[@]} -gt 0 ] || die "--admin-allow is required: the admin web answers only from the networks you list (an address or a CIDR block). It is the interim control until 2FA."
for a in "${ALLOW[@]}"; do [[ "$a" =~ ^[0-9]{1,3}(\.[0-9]{1,3}){3}(/[0-9]{1,2})?$ ]] || die "--admin-allow $a is not an IPv4 address or block"; [ "$a" != "0.0.0.0/0" ] || die "--admin-allow 0.0.0.0/0 would open the admin web to everyone"; done
if [ "$STAGING" = 1 ]; then ACME_DIR="https://acme-staging-v02.api.letsencrypt.org/directory"; else ACME_DIR="https://acme-v02.api.letsencrypt.org/directory"; fi

mkdir -p state/{secrets,etc,pgtls,keys,tls,edge/acme,edge/certs,run}
rand() { head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n'; }
for s in pg_postgres pg_app pg_admin pg_migrator; do [ -s "state/secrets/$s" ] || (umask 077; rand > "state/secrets/$s"); done

# The database CA and the server certificate for the name `db` (clients verify it: sslmode verify-full). Made once; openssl comes from the postgres image.
in_pg() { docker run --rm --entrypoint sh -v "$PWD/state/pgtls:/w" -w /w "$PG_IMG" -c "$1"; }
if [ ! -s state/pgtls/server.key ]; then
  in_pg 'set -e; umask 077
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-384 -nodes -days 3650 -subj "/CN=rusty db CA" -keyout ca.key -out ca.pem
    openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-384 -nodes -subj "/CN=db" -keyout server.key -out server.csr
    printf "subjectAltName=DNS:db\nextendedKeyUsage=serverAuth\n" > ext.cnf
    openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 3650 -extfile ext.cnf -out server.crt
    rm -f server.csr ext.cnf ca.srl; chmod 0644 ca.pem server.crt'
fi
cp state/pgtls/ca.pem state/etc/db-ca.pem

render() { # template -> file: the values were validated above, none holds a character that matters to sed (| is never in them)
  sed -e "s|@MAIL_HOST@|$MAIL_HOST|g" -e "s|@ADMIN_HOST@|$ADMIN_HOST|g" -e "s|@CONTACT@|$CONTACT|g" -e "s|@PUBLIC_IP@|$PUBLIC_IP|g" \
      -e "s|@SEAL_PUBLIC@|$SEAL_PUBLIC|g" -e "s|@ACME_DIRECTORY@|$ACME_DIR|g" -e "s|@ADMIN_ALLOW@|${ALLOW[*]}|g" "templates/$1.in" > "state/etc/$1"
}
# The sealing key pair: the private half stays in state/keys (mail uid only); the public half goes to both configurations.
mail_ctl() { docker run --rm --user 65532:65532 --entrypoint /usr/local/bin/rustyctl -v "$PWD/state/etc:/etc/rusty:ro" -v "$PWD/state/keys:/etc/rusty-keys" -v "$PWD/state/secrets/pg_app:/run/secrets/pg_app:ro" rusty-mail:local --config /etc/rusty/rusty.toml "$@"; }
[ "${SKIP_BUILD:-0}" = 1 ] || docker compose build >/dev/null   # SKIP_BUILD=1: the images rusty-mail:local and rusty-admin:local already exist
docker run --rm --user 0 --entrypoint sh -v "$PWD/state:/s" "$HA_IMG" -c 'chown 65532:65532 /s/keys /s/tls /s/secrets /s/secrets/* && chmod 0700 /s/keys /s/secrets && chmod 0600 /s/secrets/* && chmod 0755 /s/tls'
# rustyctl always loads a configuration: a bootstrap one (the template with a placeholder key) lets it make and show the key; the real one is rendered after.
SEAL_PUBLIC="rusty-seal-v1:0000000000000000000000000000000000000000000000000000000000000001"
render rusty.toml
if [ ! -e state/keys/unseal.key ]; then mail_ctl seal keygen /etc/rusty-keys/unseal.key >/dev/null; fi
SEAL_PUBLIC="$(mail_ctl seal show /etc/rusty-keys/unseal.key | grep -o 'rusty-seal-v1:[0-9a-f]\{64\}' | head -1)"
[ -n "$SEAL_PUBLIC" ] || die "could not read the sealing public key"

render rusty.toml; render admin.toml; render haproxy.cfg
# The migration run needs the migrator role; the daemon's file never names it.
{ cat state/etc/rusty.toml; echo 'migrator_user = "rusty_migrator"'; echo 'migrator_password_file = "/run/secrets/pg_migrator"'; } > state/etc/migrate.toml
chmod 0644 state/etc/*

# The edge's files belong to its user (99), the postgres key to postgres (999).
docker run --rm --user 0 --entrypoint sh -v "$PWD/state:/s" "$HA_IMG" -c 'chown -R 99:99 /s/edge /s/run && chmod 0700 /s/edge && chmod 0750 /s/run && chown -R 999:999 /s/pgtls && chmod 0600 /s/pgtls/server.key && chmod 0644 /s/pgtls/*.crt /s/pgtls/ca.pem && chmod 0600 /s/pgtls/ca.key'

cat <<DONE
setup: ready.
  start:            docker compose up -d
  first administrator: docker compose logs admin | tail -3     (the setup token), then https://$ADMIN_HOST/ from an allowed address
  DNS and mailbox:  docs/first-mail.md
$( [ "$STAGING" = 1 ] && echo "  NOTE: the staging certificate authority is not trusted by mail clients; run setup.sh again without --staging when it works." )
DONE
