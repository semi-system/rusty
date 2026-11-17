#!/usr/bin/env bash
# The two optional profiles of the swarm stack, as in the standalone stack (docs/design-filters-and-resolver.md, docs/valkey.md). Sourced by stack.sh; not run on its own.
#
#   FILTER=1   a validating resolver (unbound), the spam scorer (rspamd), the virus scanner (clamd) and its signature updater (freshclam). The scanners sit on an internal network with no route out; the
#              resolver and freshclam also join `backend`. clamd and freshclam share a local volume, so both are pinned to the node labelled av=true.
#   VALKEY=1   a shared key-value store (TLS with a CA made here, one ACL user per service, no persistence) for the sending budget, the login throttle and the admin sessions of every replica.
#
# The filters are found by NAME (clamd, rspamd). The one thing that needs an address is the resolver: rspamd and smtp-in take their nameserver as an address, so the tool reads the virtual address
# the swarm gave the resolver service on the filter network (it stays the same as long as the service exists) and writes it into their configuration; `init` therefore starts the resolver first.

FILTER="${FILTER:-0}"; VALKEY="${VALKEY:-0}"; POP3="${POP3:-0}"   # POP3=1: the IMAP service also listens for POP3 (the edge passes 110 and ends TLS on 995; docs/design-pop3.md)
KV_ROLES=(submission imap admin)

# ---- the resolver's address ---------------------------------------------------------------------------------------------------------------------------------------------------------------------
resolver_addr() { # -> 10.0.5.2:53 (empty when the resolver is not there yet)
  local net vip
  net="$(sw network inspect "${STACK_NAME}_filter" --format '{{.Id}}' 2>/dev/null || true)"
  [ -n "$net" ] || return 0
  vip="$(sw service inspect "${STACK_NAME}_resolver" --format '{{range .Endpoint.VirtualIPs}}{{.NetworkID}} {{.Addr}}{{"\n"}}{{end}}' 2>/dev/null | awk -v n="$net" '$1 == n { print $2 }' | head -1)"
  [ -n "$vip" ] || return 0
  printf '%s:53\n' "${vip%%/*}"
}

# ---- configuration blocks ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------
blk_filter() { # the scanners of smtp-in, and the resolver its DNS checks use
  [ "$FILTER" = 1 ] || return 0
  printf '[filter]\nclamd_addr = "clamd:3310"\nrspamd_url = "http://rspamd:11333"\n\n'
  local a; a="$(resolver_addr)"
  [ -z "$a" ] || printf '[resolver]\nnameserver = "%s"\n\n' "$a"
}
blk_valkey() { # ROLE
  [ "$VALKEY" = 1 ] || return 0
  printf '[valkey]\nhost = "valkey"\ntls = true\nca_bundle = "/etc/rusty/kv-ca.pem"\nusername = "rusty_%s"\npassword_file = "/run/secrets/kv_%s"\nkey_prefix = "rusty:"\n\n' "$1" "$1"
}

# The filters' own files: unbound answers the private ranges only; rspamd asks the resolver (by address, see above); clamd as in the standalone stack.
conf_unbound() {
  sed -e 's|^    access-control: 10.78.0.0/24 allow|    access-control: 10.0.0.0/8 allow\n    access-control: 172.16.0.0/12 allow\n    access-control: 192.168.0.0/16 allow|' "$HERE/../standalone/filter/unbound.conf"
}
conf_rspamd_options() {
  local a; a="$(resolver_addr)"; a="${a:-127.0.0.1:53}"
  printf 'dns {\n  nameserver = ["%s"];\n  timeout = 3s;\n  retransmits = 2;\n}\n' "$a"
}
conf_valkey_acl() {
  local r h
  printf 'user default off\n'
  for r in "${KV_ROLES[@]}"; do
    h="$(printf '%s' "$(cat "$STATE_DIR/secrets/kv_$r")" | sha256sum | cut -d' ' -f1)"
    printf 'user rusty_%s on #%s ~rusty:* +@all -@dangerous -@admin\n' "$r" "$h"
  done
}

# ---- secrets and configs --------------------------------------------------------------------------------------------------------------------------------------------------------------------------
# The CA and server certificate of the key-value store (the name `valkey`), made once, and the three passwords.
make_valkey_secrets() {
  local r
  for r in "${KV_ROLES[@]}"; do [ -s "$STATE_DIR/secrets/kv_$r" ] || ( umask 077; rand_hex 24 > "$STATE_DIR/secrets/kv_$r" ); done
  make_ca_signed "$STATE_DIR/kvtls" "rusty kv CA" valkey
}

profile_secrets() {
  local r
  if [ "$VALKEY" = 1 ]; then
    make_valkey_secrets
    for r in "${KV_ROLES[@]}"; do ensure_secret "kv_$r" "$STATE_DIR/secrets/kv_$r"; done
    ensure_secret kv_server_crt "$STATE_DIR/kvtls/server.crt"
    ensure_secret kv_server_key "$STATE_DIR/kvtls/server.key"
  fi
}

profile_configs() {
  local d="$STATE_DIR/stack/etc" f
  if [ "$VALKEY" = 1 ]; then
    ensure_config kv_ca "$STATE_DIR/kvtls/ca.pem"
    ( umask 077; conf_valkey_acl > "$d/valkey.acl" ); ensure_config valkey_acl "$d/valkey.acl"
  fi
  if [ "$FILTER" = 1 ]; then
    ( umask 077; conf_unbound > "$d/unbound.conf" ); ensure_config unbound_conf "$d/unbound.conf"
    ensure_config clamd_conf "$HERE/../standalone/filter/clamd.conf"
    for f in actions.conf milter_headers.conf redis.conf worker-controller.inc worker-normal.inc; do ensure_config "rspamd_$(printf '%s' "$f" | tr '.-' '__')" "$HERE/../standalone/filter/rspamd/$f"; done
    ( umask 077; conf_rspamd_options > "$d/options.inc" ); ensure_config rspamd_options_inc "$d/options.inc"
  fi
}

profile_names_needed() { # secrets then configs the stack file will name (used to check the Manager has them)
  local r
  if [ "$VALKEY" = 1 ]; then
    for r in "${KV_ROLES[@]}"; do printf 'S kv_%s\n' "$r"; done
    printf 'S kv_server_crt\nS kv_server_key\nC kv_ca\nC valkey_acl\n'
  fi
  if [ "$FILTER" = 1 ]; then printf 'C unbound_conf\nC clamd_conf\nC rspamd_options_inc\n'; fi
}

# ---- the services ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------
gen_resolver_service() {
  # The resolver: validating, DNSSEC, answers the private ranges only. Port 53 needs the capability to bind below 1024. It starts first: its address goes into the scanners' configuration.
  y_service_common resolver "$(lock_ref 'klutchell/unbound:latest')"
  printf '    command: [-d, -c, /etc/unbound/unbound.conf]\n    cap_add: [NET_BIND_SERVICE]\n'
  y_deploy 1 "" start-first resolver
  printf '    configs:\n'
  y_config unbound_conf /etc/unbound/unbound.conf
  printf '    networks:\n      - filter\n      - backend\n'
}

gen_scanner_services() {
  local f
  # The spam scorer.
  y_service_common rspamd "$(lock_ref 'rspamd/rspamd:latest')"
  # shellcheck disable=SC2016 # the YAML names the variables, nothing is expanded here
  printf '      - type: tmpfs\n        target: /run/rspamd\n        tmpfs: { mode: 1777 }\n      - type: tmpfs\n        target: /var/lib/rspamd\n        tmpfs: { mode: 1777 }\n'
  y_deploy 1 "" start-first rspamd
  printf '    configs:\n'
  for f in actions.conf milter_headers.conf redis.conf worker-controller.inc worker-normal.inc; do y_config "rspamd_$(printf '%s' "$f" | tr '.-' '__')" "/etc/rspamd/local.d/$f"; done
  y_config rspamd_options_inc /etc/rspamd/local.d/options.inc
  printf '    networks:\n      - filter\n'
  # The virus scanner and its updater share the signature volume: one node (av=true).
  y_service_common clamd "$(lock_ref 'clamav/clamav:stable')" false   # ClamAV writes to /var/lock and /run/clamav: as in the standalone stack its root file system is writable
  printf '      - clamdb:/var/lib/clamav\n      - type: tmpfs\n        target: /run/clamav\n'
  printf '    cap_add: [SETUID, SETGID, CHOWN, DAC_OVERRIDE, FOWNER]\n    environment:\n      CLAMAV_NO_FRESHCLAMD: "true"\n      CLAMAV_NO_MILTERD: "true"\n'
  y_deploy 1 "node.labels.av == true" stop-first clamd
  printf '    configs:\n'
  y_config clamd_conf /etc/clamav/clamd.conf
  # The image's own check pings `localhost`, which resolves to ::1 while clamd listens on IPv4 only.
  # shellcheck disable=SC2016
  printf '    healthcheck:\n      test: [CMD-SHELL, "[ \\"$$(echo PING | nc 127.0.0.1 3310)\\" = PONG ]"]\n      interval: 30s\n      timeout: 10s\n      retries: 10\n      start_period: 120s\n    networks:\n      - filter\n'
  y_service_common freshclam "$(lock_ref 'clamav/clamav:stable')" false
  printf '      - clamdb:/var/lib/clamav\n    cap_add: [SETUID, SETGID, CHOWN, DAC_OVERRIDE, FOWNER]\n    environment:\n      CLAMAV_NO_CLAMD: "true"\n      CLAMAV_NO_MILTERD: "true"\n'
  y_deploy 1 "node.labels.av == true" stop-first freshclam
  printf '    networks:\n      - backend\n'
}

gen_valkey_service() {
  y_service_common valkey "$(lock_ref 'valkey/valkey:9-alpine')"
  # the data directory is memory only; nothing in the store is worth keeping
  printf '      - type: tmpfs\n        target: /data\n        tmpfs: { mode: 1777 }\n'
  printf '    user: "65532:65532"\n    command: [valkey-server, --port, "0", --tls-port, "6379", --tls-cert-file, /run/secrets/kv_server_crt, --tls-key-file, /run/secrets/kv_server_key, --tls-ca-cert-file, /etc/rusty/kv-ca.pem, --tls-auth-clients, "no", --aclfile, /etc/valkey/users.acl, --save, "", --appendonly, "no", --dir, /data]\n'
  y_deploy 1 "" stop-first valkey
  printf '    secrets:\n'
  y_secret kv_server_crt kv_server_crt 65532
  y_secret kv_server_key kv_server_key 65532
  printf '    configs:\n'
  y_config kv_ca /etc/rusty/kv-ca.pem
  y_config valkey_acl /etc/valkey/users.acl
  # shellcheck disable=SC2016
  printf '    healthcheck:\n      test: [CMD-SHELL, "valkey-cli --tls --cacert /etc/rusty/kv-ca.pem -p 6379 ping 2>&1 | grep -qE '"'"'PONG|NOAUTH|WRONGPASS|DENIED'"'"'"]\n      interval: 10s\n      timeout: 5s\n      retries: 6\n    networks:\n      - kv\n'
}
