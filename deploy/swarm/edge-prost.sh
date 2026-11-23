#!/usr/bin/env bash
# Prost as the edge of the swarm (docs/design-prost-edge.md). Sourced by stack.sh; not run on its own.
#
#   EDGE=haproxy       (default) HAProxy is the edge, as before.
#   EDGE=prost         Prost is the edge: it holds the host ports, and prost-certd (TLS_MODE=acme) gets and renews the certificates (HTTP-01 on port 80).
#   EDGE=prost-pilot   HAProxy stays the edge and keeps the real ports; Prost runs beside it on the ports PROST_PILOT_OFFSET above them (default 20000: 20025, 20587, ...), with the certificates of
#                      HAProxy (CERT_S3=1: the ones `certs sync` keeps in the bucket) or the self-signed ones (TLS_MODE=self). It gets no certificate of its own and answers no ACME challenge.
#
#   CERT_S3=1          `certs sync` also keeps the certificates in an S3 bucket meant for administration, not for mail (default: the stack's own S3, bucket `certs`): s3://BUCKET/mail/{fullchain,privkey}.pem,
#                      s3://BUCKET/admin/... (HAProxy) or s3://BUCKET/edge/... (Prost). Prost reads them with its s3:// URIs (SigV4, no polling; a new certificate is loaded by a deploy or `prostctl tls`).
#   CERT_S3_ENDPOINT CERT_S3_REGION CERT_S3_BUCKET CERT_S3_ACCESS_KEY CERT_S3_SECRET_KEY_FILE   another S3 for them (the defaults are the stack's S3 and its keys)
#   PROST_IMAGE        the image (default: ghcr.io/semi-system/prost:latest, by the digest of images.lock when the kit has one)

EDGE="${EDGE:-haproxy}"
# 1: the edge sends the PROXY header to the admin web and the admin web reads it (real client address in the audit log and the sign-in throttle). Needs the admin image of this kit's release.
ADMIN_PROXY="${ADMIN_PROXY:-0}"
# What one client address may do at the Prost edge (HAProxy has no such limit at the edge; the services have their own): concurrent connections, and for SMTP new connections per second with a burst.
# Raise them for a site where many users sit behind one address (an office NAT).
PROST_MAX_PER_IP="${PROST_MAX_PER_IP:-50}"
PROST_CONN_RATE="${PROST_CONN_RATE:-10}"
PROST_CONN_BURST="${PROST_CONN_BURST:-100}"
# Failures (a connection that could not be made or was lost) of one service task within 5 s before the edge stops using it until a health probe passes.
PROST_MAX_FAILS="${PROST_MAX_FAILS:-20}"
PROST_PILOT_OFFSET="${PROST_PILOT_OFFSET:-20000}"
CERT_S3="${CERT_S3:-0}"
CERT_S3_BUCKET="${CERT_S3_BUCKET:-certs}"
CERT_S3_ENDPOINT="${CERT_S3_ENDPOINT:-}"
CERT_S3_REGION="${CERT_S3_REGION:-}"
CERT_S3_ACCESS_KEY="${CERT_S3_ACCESS_KEY:-}"
CERT_S3_SECRET_KEY_FILE="${CERT_S3_SECRET_KEY_FILE:-}"
PROST_IMAGE="${PROST_IMAGE:-}"

# The ports of the edge: the container's port, then the standard one it stands for.
PROST_PORTS="10025:25 10587:587 10143:143 10110:110 10465:465 10993:993 10995:995 10443:443 10080:80"

# Is HAProxy a service of this stack? (Not when Prost is the edge; with the pilot both are.)
haproxy_is_edge() { [ "$EDGE" != prost ]; }
# The name of the Prost edge service: `edge` when it is the edge (the services trust that name), `edge-prost` beside HAProxy.
prost_service() { if [ "$EDGE" = prost-pilot ]; then echo edge-prost; else echo edge; fi; }
# The services accept the PROXY header from the edge by its name: with the pilot, from Prost's as well.
edge_hosts_filter() {
  if [ "$EDGE" = prost-pilot ]; then sed 's|\["tasks.edge", "edge"\]|["tasks.edge", "edge", "tasks.edge-prost", "edge-prost"]|'; else cat; fi
}

prost_image() {
  local pinned
  if [ -n "$PROST_IMAGE" ]; then printf '%s\n' "$PROST_IMAGE"; return 0; fi
  pinned="$(lock_ref 'ghcr.io/semi-system/prost:latest')"
  if [ -n "$pinned" ]; then printf '%s\n' "$pinned"; else printf 'ghcr.io/semi-system/prost:latest\n'; fi
}

# The port of the edge as a client sees it (the pilot's are offset).
edge_port() { # STANDARD-PORT
  if [ "$EDGE" = prost-pilot ]; then echo $(( $1 + PROST_PILOT_OFFSET )); else echo "$1"; fi
}

# Does this inventory take the certificates of the edge from S3?
prost_certs_from_s3() { [ "$EDGE" = prost-pilot ] && [ "$CERT_S3" = 1 ] && [ "$TLS_MODE" = acme ]; }
# Does prost-certd run (Prost gets its own certificates)?
prost_has_certd() { [ "$EDGE" = prost ] && [ "$TLS_MODE" = acme ]; }

check_edge_inputs() {
  case "$EDGE" in haproxy|prost|prost-pilot) ;; *) die "EDGE is haproxy (default), prost or prost-pilot" ;; esac
  case "$CERT_S3" in 0|1) ;; *) die "CERT_S3 is 0 or 1" ;; esac
  case "$ADMIN_PROXY" in 0|1) ;; *) die "ADMIN_PROXY is 0 or 1" ;; esac
  local v
  for v in PROST_MAX_PER_IP PROST_CONN_RATE PROST_CONN_BURST PROST_MAX_FAILS; do [[ "${!v}" =~ ^[0-9]{1,6}$ ]] && [ "${!v}" -ge 1 ] || die "$v is a number from 1 to 999999"; done
  [[ "$PROST_PILOT_OFFSET" =~ ^[0-9]{4,5}$ ]] && [ "$PROST_PILOT_OFFSET" -ge 10000 ] && [ "$PROST_PILOT_OFFSET" -le 60000 ] || die "PROST_PILOT_OFFSET is a number from 10000 to 60000 (so that every port stays below 65536)"
  [ -z "$PROST_IMAGE" ] || [[ "$PROST_IMAGE" =~ ^[a-z0-9][a-z0-9./_-]*(:[A-Za-z0-9._-]+|@sha256:[0-9a-f]{64})$ ]] || die "PROST_IMAGE is an image name with a tag or a digest"
  if [ "$EDGE" = prost-pilot ] && [ "$TLS_MODE" = acme ] && [ "$CERT_S3" != 1 ]; then
    die "EDGE=prost-pilot with TLS_MODE=acme takes HAProxy's certificates from S3: set CERT_S3=1 (or use TLS_MODE=self for a pilot with self-signed ones)"
  fi
  if [ "$CERT_S3" = 1 ]; then
    : "${CERT_S3_ENDPOINT:=$S3_ENDPOINT}" "${CERT_S3_REGION:=$S3_REGION}" "${CERT_S3_ACCESS_KEY:=$S3_ACCESS_KEY}" "${CERT_S3_SECRET_KEY_FILE:=$S3_SECRET_KEY_FILE}"
    [[ "$CERT_S3_ENDPOINT" =~ ^https?://[A-Za-z0-9._:-]+$ ]] || die "CERT_S3_ENDPOINT is http(s)://host[:port], nothing more"
    [[ "$CERT_S3_REGION" =~ ^[a-z0-9-]{2,32}$ ]] || die "CERT_S3_REGION is lower case letters, digits and -"
    [[ "$CERT_S3_BUCKET" =~ ^[a-z0-9][a-z0-9.-]{2,62}$ ]] || die "CERT_S3_BUCKET is a bucket name (lower case letters, digits, . and -)"
    [[ "$CERT_S3_ACCESS_KEY" =~ ^[A-Za-z0-9+/=_-]{3,128}$ ]] || die "CERT_S3_ACCESS_KEY has a character that does not belong in a key"
    [ -r "$CERT_S3_SECRET_KEY_FILE" ] || die "CERT_S3_SECRET_KEY_FILE is not a readable file"
    [ "$CERT_S3_BUCKET" != "$S3_BUCKET" ] || die "CERT_S3_BUCKET must not be the bucket of the mail (it is for administration)"
  fi
}

# ---- the configuration files ----------------------------------------------------------------------------------------------------------------------------------------------------------------
# Where Prost reads the certificates from: the files of prost-certd, S3, or Docker secrets (self-signed).
prost_cert_paths() { # -> MAIL_CERT MAIL_KEY ADMIN_CERT ADMIN_KEY (globals)
  if prost_has_certd; then
    MAIL_CERT=/certificates/edge/current/fullchain.pem; MAIL_KEY=/certificates/edge/current/privkey.pem
    ADMIN_CERT=$MAIL_CERT; ADMIN_KEY=$MAIL_KEY
  elif prost_certs_from_s3; then
    MAIL_CERT="s3://$CERT_S3_BUCKET/mail/fullchain.pem"; MAIL_KEY="s3://$CERT_S3_BUCKET/mail/privkey.pem"
    ADMIN_CERT="s3://$CERT_S3_BUCKET/admin/fullchain.pem"; ADMIN_KEY="s3://$CERT_S3_BUCKET/admin/privkey.pem"
  else
    MAIL_CERT=/run/secrets/edge_mail_crt; MAIL_KEY=/run/secrets/edge_mail_key
    ADMIN_CERT=/run/secrets/edge_admin_crt; ADMIN_KEY=/run/secrets/edge_admin_key
  fi
}

# `"203.0.113.5/32", "198.51.100.0/24"` from the blank-separated ADMIN_ALLOW (each one was checked).
prost_allow_toml() { local c out=""; for c in $ADMIN_ALLOW; do out="${out:+$out, }\"$c\""; done; printf '%s' "$out"; }

# The routes of the mail host on 443: only what the mail clients' auto-setup asks for (the admin service answers them without a session).
prost_setup_routes() {
  local p
  for p in /autodiscover/autodiscover.xml /Autodiscover/Autodiscover.xml /autodiscover/autodiscover.json /mail/config.mobileconfig /mail/config-v1.1.xml /.well-known/autoconfig/mail/config-v1.1.xml; do
    printf '[[listeners.routes]]\nhost = "%s"\npath_prefix = "%s"\npriority = 100\nmethods = ["GET", "POST"]\nupstream = "web"\n\n' "$MAIL_HOST" "$p"
  done
}

conf_prost_edge() {
  prost_cert_paths
  # A line that starts with a marker is live (the marker is taken off) or is removed, as the inventory says.
  local certd_re='^#CERTD ' s3_re='^#S3 ' aproxy_re='^#ADMINPROXY '
  SETUP_ROUTES="$(prost_setup_routes)" awk '{ if ($0 == "@SETUP_ROUTES@") print ENVIRON["SETUP_ROUTES"]; else print }' "$HERE/../standalone/templates/prost-edge.toml.in" \
    | sed -e "$(if [ "$ADMIN_PROXY" = 1 ]; then echo "s|$aproxy_re||"; else echo "/$aproxy_re/d"; fi)" \
          -e "$(if prost_has_certd; then echo "s|$certd_re||"; else echo "/$certd_re/d"; fi)" \
          -e "$(if [ "$CERT_S3" = 1 ] && prost_certs_from_s3; then echo "s|$s3_re||"; else echo "/$s3_re/d"; fi)" \
          -e "s|@MAIL_CERT@|$MAIL_CERT|g" -e "s|@MAIL_KEY@|$MAIL_KEY|g" -e "s|@ADMIN_CERT@|$ADMIN_CERT|g" -e "s|@ADMIN_KEY@|$ADMIN_KEY|g" \
          -e "s|@MAX_PER_IP@|$PROST_MAX_PER_IP|g" -e "s|@CONN_RATE@|$PROST_CONN_RATE|g" -e "s|@CONN_BURST@|$PROST_CONN_BURST|g" -e "s|@PASSIVE_FAILS@|$PROST_MAX_FAILS|g" \
          -e "s|@MAIL_HOST@|$MAIL_HOST|g" -e "s|@ADMIN_HOST@|$ADMIN_HOST|g" -e "s|@ADMIN_ALLOW_TOML@|$(prost_allow_toml)|g" \
          -e "s|@CERT_S3_REGION@|${CERT_S3_REGION}|g" -e "s|@CERT_S3_ENDPOINT@|${CERT_S3_ENDPOINT}|g"
}

conf_prost_certd() {
  local dir="${ACME_DIRECTORY:-https://acme-v02.api.letsencrypt.org/directory}"
  sed -e "s|@ACME_DIRECTORY@|$dir|g" -e "s|@CONTACT@|${CONTACT:-}|g" -e "s|@MAIL_HOST@|$MAIL_HOST|g" -e "s|@ADMIN_HOST@|$ADMIN_HOST|g" "$HERE/../standalone/templates/prost-certd.toml.in"
}

# ---- prepare ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------
# The secrets of the edge: the self-signed certificates (when nothing else gives it any), and the S3 credentials of the AWS credential chain (a shared credentials file) when it reads from S3.
edge_prepare_secrets() { # STACK-DIR
  local d="$1" n
  [ "$EDGE" != haproxy ] || return 0
  if ! prost_has_certd && ! prost_certs_from_s3; then
    for n in mail admin; do
      ensure_secret "edge_${n}_crt" "$STATE_DIR/tls/$n.crt"
      ensure_secret "edge_${n}_key" "$STATE_DIR/tls/$n.key"
    done
  fi
  if prost_certs_from_s3; then
    ( umask 077; printf '[default]\naws_access_key_id = %s\naws_secret_access_key = %s\n' "$CERT_S3_ACCESS_KEY" "$(cat "$CERT_S3_SECRET_KEY_FILE")" > "$d/edge_s3_credentials" )
    ensure_secret edge_s3_credentials "$d/edge_s3_credentials"
  fi
}

edge_prepare_configs() { # ETC-DIR
  local d="$1"
  if haproxy_is_edge; then
    ( umask 077; conf_haproxy > "$d/haproxy.cfg" )
    ensure_config haproxy "$d/haproxy.cfg"
  fi
  [ "$EDGE" != haproxy ] || return 0
  ( umask 077; conf_prost_edge > "$d/prost-edge.toml" )
  ensure_config prost_edge "$d/prost-edge.toml"
  if prost_has_certd; then
    ( umask 077; conf_prost_certd > "$d/prost-certd.toml" )
    ensure_config prost_certd "$d/prost-certd.toml"
  fi
}

# The secrets and configs this inventory's edge needs, as `S name` and `C name` lines (the names load_names checks and unsets).
edge_names_needed() {
  case "$EDGE" in
    haproxy) : ;;
    *)
      echo "C prost_edge"
      if prost_has_certd; then echo "C prost_certd"; fi
      if prost_certs_from_s3; then echo "S edge_s3_credentials"; fi
      if ! prost_has_certd && ! prost_certs_from_s3; then echo "S edge_mail_crt"; echo "S edge_mail_key"; echo "S edge_admin_crt"; echo "S edge_admin_key"; fi
      ;;
  esac
}

# ---- the services --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------
# The Prost edge (and prost-certd) as services: both on the Manager (they share the admin socket and the certificate volume of that node), read-only, no capabilities, user 10001.
gen_prost_services() {
  local img pair target published svc
  img="$(prost_image)"; svc="$(prost_service)"
  y_service_common "$svc" "$img"
  # The edge's own state is rebuilt from its seed at every start (so the configuration of the stack is what runs); the certificates live in prost-certd's volume.
  # (`docker stack deploy` drops the mode of a tmpfs mount, which leaves the directory to root: a named volume of the local driver backed by tmpfs has the owner, and is empty at every start.)
  printf '      - prost_state:/var/lib/prost\n      - prost_run:/run/prost\n'
  if prost_has_certd; then printf '      - certificates:/certificates:ro\n'; fi
  # The image's own entrypoint starts Prost from the seed at /etc/prost/seed.toml (a first start reads it into the state under /var/lib/prost).
  printf '    user: "10001:10001"\n'
  prost_certs_from_s3 && printf '    environment:\n      AWS_SHARED_CREDENTIALS_FILE: /run/secrets/edge_s3_credentials\n      AWS_EC2_METADATA_DISABLED: "true"\n'
  printf '    ulimits:\n      nofile: { soft: 65536, hard: 65536 }\n    ports:\n'
  for pair in $PROST_PORTS; do
    target="${pair%%:*}"; published="${pair##*:}"
    [ "$EDGE" != prost-pilot ] || published=$(( published + PROST_PILOT_OFFSET ))
    printf '      - { target: %s, published: %s, protocol: tcp, mode: host }\n' "$target" "$published"
  done
  y_deploy 1 "node.role == manager" stop-first "$svc"
  if ! prost_has_certd; then
    printf '    secrets:\n'
    if prost_certs_from_s3; then
      y_secret edge_s3_credentials edge_s3_credentials 10001
    else
      y_secret edge_mail_crt edge_mail_crt 10001; y_secret edge_mail_key edge_mail_key 10001
      y_secret edge_admin_crt edge_admin_crt 10001; y_secret edge_admin_key edge_admin_key 10001
    fi
  fi
  printf '    configs:\n'
  y_config prost_edge /etc/prost/seed.toml
  printf '    healthcheck:\n      test: ["CMD", "/usr/local/bin/prostctl", "status"]\n      interval: 10s\n      timeout: 5s\n      retries: 3\n    networks:\n      - app\n      - backend\n'
  if prost_has_certd; then
    y_service_common certd "$img"
    printf '      - certd_state:/var/lib/prost-certd\n      - certificates:/certificates\n      - prost_run:/run/prost\n'
    printf '    user: "10001:10001"\n    entrypoint: ["/usr/local/bin/prost-certd"]\n    command: ["run", "--config", "/etc/prost/certd.toml"]\n'
    y_deploy 1 "node.role == manager" stop-first certd
    printf '    configs:\n'
    y_config prost_certd /etc/prost/certd.toml
    printf '    healthcheck:\n      test: ["CMD", "/usr/local/bin/prost-certd", "check"]\n      interval: 10s\n      timeout: 5s\n      retries: 6\n    networks:\n      - backend\n'
  fi
}

edge_volumes() {
  printf '  prost_run:\n  prost_state:\n    driver: local\n    driver_opts: { type: tmpfs, device: tmpfs, o: "uid=10001,gid=10001,mode=0700,size=64m" }\n'
  if prost_has_certd; then printf '  certd_state:\n  certificates:\n'; fi
}

# ---- certificates ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------
certd_container() { sw ps -q --filter "label=com.docker.swarm.service.name=${STACK_NAME}_certd" --filter status=running | head -1; }

# Put a certificate and its key into the bucket (s3://BUCKET/NAME/fullchain.pem and privkey.pem). curl does the SigV4 signing in a throw-away container on the Manager that reaches the S3 the way the
# services do (the stack's network); the files and the credentials are in a private directory there for the length of the call.
cert_s3_put() { # NAME CERT-FILE KEY-FILE
  local name="$1" crt="$2" key="$3" mgr tmp uid obj rc=0 net=()
  mgr="$(manager_name)"
  tmp="$(on "$mgr" mktemp -d)"
  uid="$(on "$mgr" id -u):$(on "$mgr" id -g)"
  on "$mgr" sh -c "umask 077; cat > $tmp/crt" < "$crt"
  on "$mgr" sh -c "umask 077; cat > $tmp/key" < "$key"
  printf 'user = "%s:%s"\n' "$CERT_S3_ACCESS_KEY" "$(cat "$CERT_S3_SECRET_KEY_FILE")" | on "$mgr" sh -c "umask 077; cat > $tmp/curl.cfg"
  if sw network inspect "${STACK_NAME}_backend" >/dev/null 2>&1; then net=(--network "${STACK_NAME}_backend"); fi
  # The bucket first (it exists after the first call: the answer for that is not an error).
  sw run --rm --cap-drop ALL --read-only --user "$uid" "${net[@]}" -v "$tmp:/c:ro" "$(lock_ref 'curlimages/curl:8.15.0')" -sS -o /dev/null -K /c/curl.cfg --aws-sigv4 "aws:amz:$CERT_S3_REGION:s3" -X PUT \
    "$CERT_S3_ENDPOINT/$CERT_S3_BUCKET" >/dev/null 2>&1 || true
  for obj in crt:fullchain.pem key:privkey.pem; do
    sw run --rm --cap-drop ALL --read-only --user "$uid" "${net[@]}" -v "$tmp:/c:ro" "$(lock_ref 'curlimages/curl:8.15.0')" -sS --fail-with-body --connect-timeout 10 --max-time 60 -K /c/curl.cfg \
      --aws-sigv4 "aws:amz:$CERT_S3_REGION:s3" -T "/c/${obj%%:*}" "$CERT_S3_ENDPOINT/$CERT_S3_BUCKET/$name/${obj##*:}" >/dev/null || rc=1
  done
  on "$mgr" rm -rf "$tmp"
  [ "$rc" = 0 ] || { echo "certs sync: the $name certificate could not be put in s3://$CERT_S3_BUCKET/$name/" >&2; return 1; }
  echo "certs sync: s3://$CERT_S3_BUCKET/$name/ has the current $name certificate"
}

# EDGE=prost with ACME: prost-certd's certificate to the mail services (the Docker secrets tls_cert and tls_key), as `certs sync` does for HAProxy's. The image has no shell: `docker cp` reads the files.
cmd_certs_sync_prost() {
  local c dir="$STATE_DIR/tls" changed=0 pem_dir tmp
  read_names || true
  c="$(certd_container)"
  [ -n "$c" ] || { echo "certs sync: prost-certd is not running yet" >&2; return 1; }
  mkdir -p "$dir"; chmod 0700 "$dir"
  tmp="$(mktemp -d)"; chmod 0700 "$tmp"; pem_dir="$tmp"
  if ! sw cp "$c:/certificates/edge/current/fullchain.pem" - 2>/dev/null | tar -xO > "$pem_dir/fullchain.pem" || ! sw cp "$c:/certificates/edge/current/privkey.pem" - 2>/dev/null | tar -xO > "$pem_dir/privkey.pem" \
     || ! grep -q 'BEGIN CERTIFICATE' "$pem_dir/fullchain.pem" || ! grep -q 'PRIVATE KEY' "$pem_dir/privkey.pem"; then
    rm -rf "$tmp"; echo "certs sync: prost-certd has no certificate for $MAIL_HOST yet" >&2; return 1
  fi
  if ! cmp -s "$pem_dir/fullchain.pem" "$dir/mail.crt"; then
    ( umask 077; cp "$pem_dir/privkey.pem" "$dir/mail.key"; cp "$pem_dir/fullchain.pem" "$dir/mail.crt" )
    ensure_secret tls_cert "$dir/mail.crt"
    ensure_secret tls_key "$dir/mail.key"
    save_names
    changed=1
  fi
  if [ "$CERT_S3" = 1 ]; then cert_s3_put edge "$pem_dir/fullchain.pem" "$pem_dir/privkey.pem" || true; fi
  rm -rf "$tmp"
  if [ "$changed" = 1 ]; then echo "certs sync: a new certificate for $MAIL_HOST; the mail services move onto it"; cmd_deploy; else echo "certs sync: the certificates are current"; fi
}

# What the edge presents: the Prost status and, for prost-certd, its certificate report (expiry and the next renewal), read inside the containers.
cmd_edge_status() {
  local c
  c="$(sw ps -q --filter "label=com.docker.swarm.service.name=${STACK_NAME}_$(prost_service)" --filter status=running | head -1)"
  [ -n "$c" ] || die "edge status: the edge is not running"
  echo "edge: $EDGE"
  if [ "$EDGE" = haproxy ]; then echo "(HAProxy: see 'logs edge')"; return 0; fi
  sw exec "$c" /usr/local/bin/prostctl status || true
  if prost_has_certd; then
    c="$(certd_container)"
    [ -z "$c" ] || sw exec "$c" /usr/local/bin/prost-certd check --certificates || echo "edge status: prost-certd says a certificate is not ready yet" >&2
  fi
}

# The volumes of the edge belong to the user 10001 (the image's user, which has no shell and cannot chown): a new named volume is made here and handed over by a throw-away container as root with
# only the CHOWN capability, before the stack is deployed. Without this Prost cannot create its admin socket in /run/prost ("Permission denied").
edge_prepare_volumes() {
  local v
  [ "$EDGE" != haproxy ] || return 0
  for v in prost_run $(prost_has_certd && echo certd_state certificates); do
    sw volume create --label "com.docker.stack.namespace=$STACK_NAME" "${STACK_NAME}_$v" >/dev/null
    sw run --rm --user 0 --cap-drop ALL --cap-add CHOWN --entrypoint chown -v "${STACK_NAME}_$v:/v" "$(lock_ref 'curlimages/curl:8.15.0')" 10001:10001 /v \
      || die "the volume ${STACK_NAME}_$v could not be handed to the edge's user"
  done
}
