#!/usr/bin/env bash
# The DNS check before the setup (sourced by rusty-swarm). With TLS_MODE=acme the certificates are ordered from Let's Encrypt, which looks the host names up and connects to the address they name: if they do not
# point at this server yet, the edge orders in vain (and a failed order counts against the limits of the certificate authority). So `init` first checks that MAIL_HOST, ADMIN_HOST and WEBMAIL_HOST (when set)
# resolve to the address of the Manager, and WAITS until they do: the operator makes the records in the meantime and the setup goes on by itself when they are there.
#
#   ./rusty-swarm dns check     once: what each name points at; exit 1 when a name is not yet right
#   ./rusty-swarm dns wait      until every name is right (the wait `init` does)
#
# Settings of swarm.env: DNS_CHECK=auto|1|0 (auto: on with TLS_MODE=acme, off with self-signed certificates), DNS_WAIT_MINUTES=120 (how long to wait before giving up; 0: do not wait, fail),
# DNS_EXPECT_IP=ADDRESS (the address the names must point at: default the public address of the Manager in the inventory, else the address this machine reaches the Internet from).
# The names are asked of public resolvers (1.1.1.1 and 8.8.8.8) when `dig` is here, else of this machine's resolver; the answer of ANY of them counts, so a record that has just been made shows up soon.

DNS_CHECK="${DNS_CHECK:-auto}"
DNS_WAIT_MINUTES="${DNS_WAIT_MINUTES:-120}"
DNS_EXPECT_IP="${DNS_EXPECT_IP:-}"
DNS_POLL_SECS="${DNS_POLL_SECS:-15}"

# The addresses (A and AAAA) one name has, one per line, empty when it has none.
dns_lookup() {
  local name="$1" r t out=""
  if command -v dig >/dev/null 2>&1; then
    for r in 1.1.1.1 8.8.8.8; do
      for t in A AAAA; do
        out="$out$(dig +short +time=3 +tries=1 "$t" "$name" "@$r" 2>/dev/null | grep -E '^[0-9a-fA-F:.]+$' || true)"$'\n'
      done
    done
  fi
  if [ -z "${out//[$'\n']/}" ] && command -v getent >/dev/null 2>&1; then
    out="$(getent ahosts "$name" 2>/dev/null | awk '{ print $1 }' || true)"
  fi
  printf '%s\n' "$out" | grep -v '^$' | sort -u || true
}

# The address the names must point at.
dns_expected_ip() {
  local a="$DNS_EXPECT_IP"
  if [ -z "$a" ]; then a="$(node_field "$(manager_name)" 5 2>/dev/null || true)"; fi
  # A name in the inventory field is looked up; an address that is private (a machine behind a NAT) is not what the world sees.
  case "$a" in
    10.*|192.168.*|172.1[6-9].*|172.2[0-9].*|172.3[0-1].*|127.*) a="" ;;
  esac
  if [ -z "$a" ] && command -v curl >/dev/null 2>&1; then
    a="$(curl -fsS --max-time 6 https://api.ipify.org 2>/dev/null || true)"
    [[ "$a" =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ ]] || a=""
  fi
  printf '%s\n' "$a"
}

# The names this installation needs.
dns_names() {
  printf '%s\n%s\n' "$MAIL_HOST" "$ADMIN_HOST"
  # The webmail on the mail host (WEBMAIL_HOST=MAIL_HOST) is not one more name.
  if [ -n "${WEBMAIL_HOST:-}" ] && [ "$WEBMAIL_HOST" != "$MAIL_HOST" ]; then printf '%s\n' "$WEBMAIL_HOST"; fi
}

# Is this an address of Cloudflare (a name whose record is "Proxied" there answers with these, not with the address of the server)?
dns_is_cloudflare() {
  [[ "$1" =~ ^(104\.(1[6-9]|2[0-9]|3[01])\.|172\.(6[4-9]|7[01])\.|162\.15[89]\.|108\.162\.|141\.101\.(6[4-9]|[7-9][0-9]|1[01][0-9]|12[0-7])\.|188\.114\.(9[6-9]|10[0-9]|11[01])\.|190\.93\.(24[0-9]|25[0-5])\.|197\.234\.24[0-3]\.|198\.41\.(12[89]|1[3-9][0-9]|2[0-9][0-9])\.|103\.21\.24[4-7]\.|103\.22\.20[0-3]\.|103\.31\.[4-7]\.|131\.0\.7[2-5]\.|173\.245\.(4[89]|5[0-9]|6[0-3])\.) ]] \
    || [[ "$1" =~ ^(2606:4700:|2803:f800:|2405:b500:|2405:8100:|2a06:98c) ]]
}

# One pass: prints a line per name, returns 0 when every name points at the expected address (or, with no expected address known, resolves at all). The lines name the TYPE of the record
# to make (an A record for an IPv4 address, AAAA for IPv6): the name of a host (mx.example.org) is not the MX record of the domain.
dns_pass() {
  local want="$1" n got good=0 total=0 type="A" short a hint
  case "$want" in *:*) type="AAAA" ;; esac
  while read -r n; do
    [ -n "$n" ] || continue
    total=$((total + 1))
    short="${n%%.*}"
    got="$(dns_lookup "$n")"
    if [ -z "$got" ]; then
      printf '  WAIT  %s: no address record yet. Make the %s record  %s  ->  %s\n' "$n" "$type" "$short" "${want:-the address of this server}"
    elif [ -z "$want" ]; then
      printf '  ok    %s -> %s (the address of this server is not known: only that the name exists is checked)\n' "$n" "$(printf '%s' "$got" | tr '\n' ' ')"
      good=$((good + 1))
    elif printf '%s\n' "$got" | grep -qxF "$want"; then
      printf '  ok    %s (%s record) -> %s\n' "$n" "$type" "$want"
      good=$((good + 1))
    else
      hint=""
      for a in $got; do
        if dns_is_cloudflare "$a"; then hint=" These are Cloudflare's addresses: the record is \"Proxied\"; set it to \"DNS only\" (grey cloud) and put $want in it."; break; fi
      done
      printf '  WAIT  %s: its address records point at %s, not at %s. Change the %s record  %s  to %s.%s\n' "$n" "$(printf '%s' "$got" | tr '\n' ' ' | sed 's/ $//')" "$want" "$type" "$short" "$want" "$hint"
    fi
  done < <(dns_names)
  [ "$good" = "$total" ]
}

dns_enabled() {
  case "$DNS_CHECK" in
    1) return 0 ;;
    0) return 1 ;;
    *) [ "$TLS_MODE" = acme ] ;;
  esac
}

cmd_dns_check() {
  local want
  want="$(dns_expected_ip)"
  echo "dns check: the names must point at ${want:-this server (its public address is not known)}"
  dns_pass "$want" || { echo "dns check: not yet" >&2; return 1; }
  echo "dns check: the names are right"
}

# Wait until the names are right; the operator makes the records meanwhile. Gives up after DNS_WAIT_MINUTES.
cmd_dns_wait() {
  local want deadline now last=""
  want="$(dns_expected_ip)"
  echo "dns: the names must point at ${want:-this server (its public address is not known)}:"
  if dns_pass "$want"; then echo "dns: the names are right"; return 0; fi
  [[ "$DNS_WAIT_MINUTES" =~ ^[0-9]+$ ]] || die "DNS_WAIT_MINUTES is a number of minutes"
  [ "$DNS_WAIT_MINUTES" -gt 0 ] || die "dns: the names do not point at this server yet (see WAIT above): make the records and run again"
  echo "dns: waiting for the records (up to $DNS_WAIT_MINUTES minutes; every ${DNS_POLL_SECS}s; Ctrl-C stops, run again later: nothing was changed yet)"
  deadline=$(( $(date +%s) + DNS_WAIT_MINUTES * 60 ))
  while :; do
    sleep "$DNS_POLL_SECS"
    now="$(date +%s)"
    if out="$(dns_pass "$want")"; then
      printf '%s\n' "$out"
      echo "dns: the names are right: the setup goes on"
      return 0
    fi
    # Say it again only when something changed (and every few minutes, so a quiet terminal does not look hung).
    if [ "$out" != "$last" ] || [ $((now % 300)) -lt "$DNS_POLL_SECS" ]; then
      printf '%s\n' "$out"
      last="$out"
    fi
    [ "$now" -lt "$deadline" ] || die "dns: the names still do not point at this server after $DNS_WAIT_MINUTES minutes: make the records, then run './rusty-swarm init' again"
  done
}

# The step of `init`.
wait_dns() {
  if ! dns_enabled; then echo "dns: not checked (TLS_MODE=$TLS_MODE; DNS_CHECK=1 forces it)"; return 0; fi
  cmd_dns_wait
}
