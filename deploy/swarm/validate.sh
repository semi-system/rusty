#!/usr/bin/env bash
# The values of swarm.env that end up inside generated TOML, sed expressions and SQL are checked before anything is generated, whoever wrote the file (the wizard checks its answers too; a file written
# by hand skips the wizard). Sourced by stack.sh; not run on its own.

valid_host() { [[ "$1" =~ ^[A-Za-z0-9]([A-Za-z0-9.-]{0,251}[A-Za-z0-9])?$ ]]; }
valid_ipv4() { local o; [[ "$1" =~ ^[0-9]{1,3}(\.[0-9]{1,3}){3}$ ]] || return 1; for o in ${1//./ }; do [ "$((10#$o))" -le 255 ] || return 1; done; }
valid_cidr() { # an address or an address block; never one that holds every address
  local a="${1%%/*}" m=32
  [ "$1" = "$a" ] || m="${1#*/}"
  valid_ipv4 "$a" && [[ "$m" =~ ^[0-9]{1,2}$ ]] && [ "$m" -le 32 ] && ! { [ "$m" = 0 ]; }
}

# check_inputs: dies with the first value that does not belong in the place it is going to.
check_inputs() {
  local c n=0
  valid_host "$MAIL_HOST" || die "MAIL_HOST is not a host name"
  valid_host "$ADMIN_HOST" || die "ADMIN_HOST is not a host name"
  if [ -n "$WEBMAIL_HOST" ]; then
    valid_host "$WEBMAIL_HOST" || die "WEBMAIL_HOST is not a host name"
    { [ "$WEBMAIL_HOST" != "$MAIL_HOST" ] && [ "$WEBMAIL_HOST" != "$ADMIN_HOST" ]; } || die "WEBMAIL_HOST is a host name of its own: not MAIL_HOST and not ADMIN_HOST"
  fi
  if [ -n "$WEBMAIL_HOST" ]; then
    valid_host "$WEBMAIL_HOST" || die "WEBMAIL_HOST is not a host name"
    [ "$WEBMAIL_HOST" != "$MAIL_HOST" ] && [ "$WEBMAIL_HOST" != "$ADMIN_HOST" ] || die "WEBMAIL_HOST is a host name of its own: not MAIL_HOST and not ADMIN_HOST"
  fi
  for c in $ADMIN_ALLOW; do
    n=$((n + 1)); valid_cidr "$c" || die "ADMIN_ALLOW: '$c' is not an IPv4 address or block (and /0 would open the admin web to everyone)"
  done
  [ "$n" -ge 1 ] && [ "$n" -le 16 ] || die "ADMIN_ALLOW names one to sixteen addresses or blocks"
  [[ "$S3_ENDPOINT" =~ ^https?://[A-Za-z0-9._:-]+$ ]] || die "S3_ENDPOINT is http(s)://host[:port], nothing more"
  [[ "$S3_REGION" =~ ^[a-z0-9-]{2,32}$ ]] || die "S3_REGION is lower case letters, digits and -"
  [[ "$S3_BUCKET" =~ ^[a-z0-9][a-z0-9.-]{2,62}$ ]] || die "S3_BUCKET is a bucket name (lower case letters, digits, . and -)"
  [[ "$S3_ACCESS_KEY" =~ ^[A-Za-z0-9+/=_-]{3,128}$ ]] || die "S3_ACCESS_KEY has a character that does not belong in a key"
  valid_host "$DB_HOST" || die "DB_HOST is not a host name"
  [[ "$DB_PORT" =~ ^[0-9]{1,5}$ ]] && [ "$DB_PORT" -ge 1 ] && [ "$DB_PORT" -le 65535 ] || die "DB_PORT is a port number"
  [[ "$DB_NAME" =~ ^[a-z][a-z0-9_]{0,62}$ ]] || die "DB_NAME is lower case letters, digits and _"
  [[ "$DB_ADMIN_USER" =~ ^[A-Za-z][A-Za-z0-9_]{0,62}$ ]] || die "DB_ADMIN_USER is a role name"
  [ -z "$DB_HOSTADDR" ] || valid_ipv4 "$DB_HOSTADDR" || die "DB_HOSTADDR is an IPv4 address"
  [ -z "${ACME_DIRECTORY:-}" ] || [[ "$ACME_DIRECTORY" =~ ^https://[A-Za-z0-9._:/-]+$ ]] || die "ACME_DIRECTORY is an https URL"
  [[ "$REPLICAS_DEFAULT" =~ ^[1-9][0-9]?$ ]] || die "REPLICAS_DEFAULT is a number from 1 to 99"
  [[ "$REPLICAS_ADMIN" =~ ^[1-9][0-9]?$ ]] || die "REPLICAS_ADMIN is a number from 1 to 99"
  [ "$REPLICAS_ADMIN" = 1 ] || [ "${VALKEY:-0}" = 1 ] || die "REPLICAS_ADMIN above 1 needs VALKEY=1: the sessions of the admin console live in the shared key-value store"
  case "$LOG_LEVEL" in error|warn|info|debug|trace) ;; *) die "LOG_LEVEL is error, warn, info, debug or trace" ;; esac
}
