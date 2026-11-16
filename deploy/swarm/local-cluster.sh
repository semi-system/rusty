#!/usr/bin/env bash
# A throw-away Docker Swarm cluster on one machine, for testing the swarm deployment (docs/design-swarm.md, section 3b): the nodes are privileged `docker:dind` containers on one Docker network.
# Run it where Docker runs (the WSL2 Ubuntu of the owner's PC, or any Linux with Docker).
#
#   ./local-cluster.sh up [--workers N]        create the Manager `rusty-m1` (a swarm of one node: the default, and what the project tests on) and, with --workers, N more nodes `rusty-w1`... joined to it
#   ./local-cluster.sh status                  the nodes, their labels and the ports the Manager publishes to this machine
#   ./local-cluster.sh verify [--encrypted]    prove the data path: a task on every node, `tasks.<service>` lists them all, a ping crosses nodes (an encrypted overlay with --encrypted)
#   ./local-cluster.sh node-stop NAME          stop a node (a node loss) / node-start NAME starts it again
#   ./local-cluster.sh docker [ARGS...]        run `docker ARGS` against the Manager (for example: ./local-cluster.sh docker node ls)
#   ./local-cluster.sh ext up | status | verify | down [--purge]
#                                              the EXTERNAL dependencies of the cluster, as plain containers outside the swarm on the network `rusty-ext` (172.30.0.0/24): a PostgreSQL 18 that accepts
#                                              TLS only (172.30.0.10, name db.rusty.test, a throw-away CA) and a RustFS S3 server (172.30.0.11:9000, bucket rusty-mail); the nodes are attached to the
#                                              network, so a task reaches them from its node's address, as it will reach a real database and a real bucket. Their settings are in
#                                              $RUSTY_LOCAL_DIR/ext (default ~/.rusty-swarm-local/ext, mode 0700): ca.pem, pg.env, s3.env
#   ./local-cluster.sh down [--purge]          remove the nodes and the network (--purge also removes their volumes)
#
# The Manager publishes the edge's ports to this machine on high ports (10025 for 25, 10080 for 80, 10143, 10443, 10465, 10587, 10993): the edge is a host-mode task on the Manager, its ports exist inside
# `rusty-m1`. Labels: `outbound=true` and `av=true` on the Manager when there is no worker (it then runs everything); with workers, `outbound=true` on the first and `av=true` on the second (the first when there is only one), standing in for the real roles.
# What a local cluster cannot prove (the encrypted overlay needs IPsec states the nested nodes may not create, real firewalls, real network timing) is for the run on real machines.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
ROOT="$(cd ../.. && pwd)"
IMAGE="$(awk '$1 == "docker:27-dind" { print $2 }' "$ROOT/deploy/images.lock")"
[ -n "$IMAGE" ] || { echo "local-cluster: docker:27-dind is not pinned in deploy/images.lock" >&2; exit 1; }
NET=rusty-cluster
MGR=rusty-m1
EDGE_PORTS=(25 80 143 443 465 587 993)

die() { echo "local-cluster: $*" >&2; exit 1; }
need_docker() { docker info >/dev/null 2>&1 || die "Docker does not answer here"; }
inner() { local node="$1"; shift; docker exec "$node" docker "$@"; }
mgr() { inner "$MGR" "$@"; }
nodes() { docker ps -a --filter "name=^rusty-[mw][0-9]*$" --format '{{.Names}}' | sort; }
wait_ready() { local n="$1"; for _ in $(seq 1 60); do inner "$n" info >/dev/null 2>&1 && return 0; sleep 2; done; die "the Docker daemon inside $n did not start"; }

start_node() { # name, extra docker run arguments...
  local name="$1"; shift
  if docker inspect "$name" >/dev/null 2>&1; then docker start "$name" >/dev/null; return; fi
  docker run -d --privileged --name "$name" --hostname "$name" --network "$NET" -e DOCKER_TLS_CERTDIR= -v "$name-lib:/var/lib/docker" "$@" "$IMAGE" >/dev/null
}

cmd_up() {
  local workers=0
  while [ $# -gt 0 ]; do
    case "$1" in
      --workers) workers="${2:-}"; shift 2 ;;
      *) die "up: unknown argument $1" ;;
    esac
  done
  [[ "$workers" =~ ^[0-9]$ ]] || die "--workers is a number from 0 to 9"
  need_docker
  docker network inspect "$NET" >/dev/null 2>&1 || docker network create "$NET" >/dev/null
  local pubs=(); for p in "${EDGE_PORTS[@]}"; do pubs+=(-p "10$(printf '%03d' "$p"):$p"); done
  # 25 becomes 10025, 80 becomes 10080, 443 becomes 10443 ...: "10" and the port padded to three digits.
  start_node "$MGR" "${pubs[@]}"
  wait_ready "$MGR"
  if [ "$(mgr info --format '{{.Swarm.LocalNodeState}}')" != active ]; then
    local ip; ip="$(docker inspect -f "{{(index .NetworkSettings.Networks \"$NET\").IPAddress}}" "$MGR")"
    mgr swarm init --advertise-addr "$ip" >/dev/null
    echo "local-cluster: the Manager started the swarm on $ip"
  fi
  local token ip; token="$(mgr swarm join-token -q worker)"; ip="$(docker inspect -f "{{(index .NetworkSettings.Networks \"$NET\").IPAddress}}" "$MGR")"
  local i
  for i in $(seq 1 "$workers"); do
    local w="rusty-w$i"
    start_node "$w"
    wait_ready "$w"
    attach_ext
    if [ "$(inner "$w" info --format '{{.Swarm.LocalNodeState}}')" != active ]; then
      inner "$w" swarm join --token "$token" "$ip:2377" >/dev/null
      echo "local-cluster: $w joined"
    fi
  done
  local ob="$MGR" av="$MGR"
  if [ "$workers" -ge 1 ]; then ob=rusty-w1; av=rusty-w1; fi
  if [ "$workers" -ge 2 ]; then av=rusty-w2; fi
  mgr node update --label-add outbound=true "$ob" >/dev/null
  mgr node update --label-add av=true "$av" >/dev/null
  cmd_status
}

cmd_status() {
  need_docker
  docker inspect "$MGR" >/dev/null 2>&1 || die "no cluster here (run: $0 up)"
  echo "nodes:"
  mgr node ls --format '  {{.Hostname}}  {{.Status}}  {{.Availability}}  {{.ManagerStatus}}'
  for n in $(mgr node ls --format '{{.Hostname}}'); do
    echo "  labels of $n: $(mgr node inspect "$n" --format '{{json .Spec.Labels}}')"
  done
  echo "ports of the Manager on this machine:"
  for p in "${EDGE_PORTS[@]}"; do printf '  %s -> localhost:10%03d\n' "$p" "$p"; done
}

# Remove the probe service and its network; the network cannot go while the last task is still shutting down, so it is tried until it does.
probe_cleanup() {
  local svc="$1" net="$2" i
  mgr service rm "$svc" >/dev/null 2>&1 || true
  for i in $(seq 1 30); do mgr network rm "$net" >/dev/null 2>&1 && return 0; mgr network inspect "$net" >/dev/null 2>&1 || return 0; sleep 2; done
  die "the probe network $net could not be removed"
}

cmd_verify() {
  local enc=0
  while [ $# -gt 0 ]; do case "$1" in --encrypted) enc=1; shift ;; *) die "verify: unknown argument $1" ;; esac; done
  need_docker
  local net=verify-net svc=verify-probe opts=(); [ "$enc" = 0 ] || opts=(--opt encrypted)
  probe_cleanup "$svc" "$net"
  mgr network create -d overlay --attachable "${opts[@]}" "$net" >/dev/null
  mgr service create --name "$svc" --mode global --network "$net" busybox sleep 3600 >/dev/null
  local total; total="$(mgr node ls -q | wc -l)"
  local up=0
  for _ in $(seq 1 60); do up="$(mgr service ps "$svc" --filter desired-state=running --format '{{.CurrentState}}' | grep -c '^Running')"; [ "$up" = "$total" ] && break; sleep 2; done
  [ "$up" = "$total" ] || { mgr service ps "$svc" --no-trunc >&2; die "verify: only $up of $total tasks run"; }
  local fail=0 n c ips o oc oip
  for n in $(nodes); do
    c="$(inner "$n" ps -q --filter "name=$svc" | head -1)"
    [ -n "$c" ] || { echo "FAIL: no task on $n"; fail=1; continue; }
    ips="$(inner "$n" exec "$c" nslookup "tasks.$svc" 2>/dev/null | awk '/^Address/ && $2 ~ /^[0-9]/ { print $2 }' | grep -vc '^127\.' || true)"
    [ "$ips" = "$total" ] || { echo "FAIL: on $n tasks.$svc lists $ips addresses, expected $total"; fail=1; }
    for o in $(nodes); do
      [ "$o" = "$n" ] && continue
      oc="$(inner "$o" ps -q --filter "name=$svc" | head -1)"; oip="$(inner "$o" exec "$oc" hostname -i | head -1)"
      if inner "$n" exec "$c" ping -c2 -W2 "$oip" >/dev/null 2>&1; then echo "ok: $n -> $o ($oip)"; else echo "FAIL: $n cannot reach the task on $o ($oip)"; fail=1; fi
    done
  done
  probe_cleanup "$svc" "$net"
  [ "$fail" = 0 ] || die "verify: the data path between the nodes does not work"
  echo "verify: every node runs a task, tasks.$svc lists all $total and every node reaches every other${opts:+ (encrypted overlay)}"
}

cmd_node() { # stop|start NAME
  need_docker
  local what="$1" name="${2:-}"
  [[ "$name" =~ ^rusty-[mw][0-9]*$ ]] || die "$what: a node name is rusty-m1, rusty-w1, ..."
  docker "$what" "$name" >/dev/null
  case "$what" in stop) echo "local-cluster: $name stopped" ;; *) echo "local-cluster: $name started" ;; esac
}

cmd_down() {
  local purge=0; [ "${1:-}" = "--purge" ] && purge=1
  need_docker
  local n
  for n in $(nodes); do docker rm -f "$n" >/dev/null; [ "$purge" = 0 ] || docker volume rm "$n-lib" >/dev/null 2>&1 || true; done
  docker network rm "$NET" >/dev/null 2>&1 || true
  if [ "$purge" = 1 ]; then echo "local-cluster: removed (volumes too)"; else echo "local-cluster: removed"; fi
}

# ---------------------------------------------------------------------------------------- the external PostgreSQL and S3

EXT_NET=rusty-ext; EXT_SUBNET=172.30.0.0/24; PG_NAME=rusty-ext-pg; PG_IP=172.30.0.10; PG_HOST=db.rusty.test; S3_NAME=rusty-ext-s3; S3_IP=172.30.0.11; BUCKET=rusty-mail
EXT_DIR="${RUSTY_LOCAL_DIR:-$HOME/.rusty-swarm-local}/ext"
PG_IMAGE="$(awk '$1 == "postgres:18" { print $2 }' "$ROOT/deploy/images.lock")"
S3_IMAGE="$(awk '$1 == "rustfs/rustfs:latest" { print $2 }' "$ROOT/deploy/images.lock")"

rand_hex() { od -An -N"$1" -tx1 /dev/urandom | tr -d ' \n'; }

attach_ext() { # the nodes join the external network (a node that is not there yet is skipped)
  docker network inspect "$EXT_NET" >/dev/null 2>&1 || return 0
  local n
  for n in $(nodes); do
    [ "$(docker inspect -f "{{if index .NetworkSettings.Networks \"$EXT_NET\"}}yes{{end}}" "$n")" = yes ] || docker network connect "$EXT_NET" "$n"
  done
}

ext_up() {
  need_docker
  # EXT_PUBLISH=1: the two services are also published on this machine's ports 5432 and 9000. Containers on the default bridge network (a CI runner with no nodes attached) cannot reach the
  # address of the user-defined network `rusty-ext`, but they can reach the host's own address.
  local PUB_PG=() PUB_S3=()
  if [ "${EXT_PUBLISH:-0}" = 1 ]; then PUB_PG=(-p 5432:5432); PUB_S3=(-p 9000:9000); fi
  [ -n "$PG_IMAGE" ] && [ -n "$S3_IMAGE" ] || die "postgres:18 and rustfs/rustfs must be pinned in deploy/images.lock"
  umask 077; mkdir -p "$EXT_DIR"; chmod 0700 "$EXT_DIR"
  docker network inspect "$EXT_NET" >/dev/null 2>&1 || docker network create --subnet "$EXT_SUBNET" "$EXT_NET" >/dev/null

  # PostgreSQL: a throw-away CA and a server certificate for the name and the address, the superuser password, TLS 1.3 only, and a pg_hba that accepts TLS connections only.
  [ -s "$EXT_DIR/pg.env" ] || printf 'PGUSER=postgres\nPGPASSWORD=%s\nPGDATABASE=rusty\nPGHOST=%s\nPGHOSTADDR=%s\n' "$(rand_hex 24)" "$PG_HOST" "$PG_IP" > "$EXT_DIR/pg.env"
  if ! docker volume inspect rusty-ext-pg-tls >/dev/null 2>&1 || [ ! -s "$EXT_DIR/ca.pem" ]; then
    docker volume rm -f rusty-ext-pg-tls >/dev/null 2>&1 || true
    docker run --rm -v rusty-ext-pg-tls:/tls --entrypoint sh "$PG_IMAGE" -c "set -e; umask 077; cd /tls
      openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-384 -nodes -days 3650 -subj '/CN=rusty local CA' -keyout ca.key -out ca.pem 2>/dev/null
      openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-384 -nodes -subj '/CN=$PG_HOST' -keyout server.key -out server.csr 2>/dev/null
      printf 'subjectAltName=DNS:$PG_HOST,IP:$PG_IP\nextendedKeyUsage=serverAuth\n' > ext.cnf
      openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 3650 -extfile ext.cnf -out server.crt 2>/dev/null
      rm -f server.csr ext.cnf ca.srl ca.key; chown 999:999 /tls/server.key /tls/server.crt /tls/ca.pem; chmod 0600 /tls/server.key; chmod 0644 /tls/server.crt /tls/ca.pem"
    docker run --rm -v rusty-ext-pg-tls:/tls:ro --entrypoint cat "$PG_IMAGE" /tls/ca.pem > "$EXT_DIR/ca.pem"
  fi
  printf 'local all all trust\nhostssl all all all scram-sha-256\nhost all all all reject\n' > "$EXT_DIR/pg_hba.conf"; chmod 0644 "$EXT_DIR/pg_hba.conf"
  # The password is read from a file by the image's entrypoint; the file is not in the environment of the container.
  sed -n 's/^PGPASSWORD=//p' "$EXT_DIR/pg.env" > "$EXT_DIR/pg.pw"; chmod 0644 "$EXT_DIR/pg.pw"
  if ! docker inspect "$PG_NAME" >/dev/null 2>&1; then
    docker run -d --name "$PG_NAME" --network "$EXT_NET" --ip "$PG_IP" --restart unless-stopped "${PUB_PG[@]}" \
      -e POSTGRES_PASSWORD_FILE=/run/pg.pw -e POSTGRES_DB=rusty -e POSTGRES_INITDB_ARGS="--auth-host=scram-sha-256 --data-checksums" \
      -v rusty-ext-pg-data:/var/lib/postgresql -v rusty-ext-pg-tls:/tls:ro -v "$EXT_DIR/pg.pw:/run/pg.pw:ro" -v "$EXT_DIR/pg_hba.conf:/etc/pg_hba.conf:ro" \
      "$PG_IMAGE" postgres -c listen_addresses='*' -c ssl=on -c ssl_cert_file=/tls/server.crt -c ssl_key_file=/tls/server.key -c ssl_min_protocol_version=TLSv1.3 -c hba_file=/etc/pg_hba.conf >/dev/null
  else
    docker start "$PG_NAME" >/dev/null 2>&1 || true
  fi

  # RustFS (S3): a key pair, a data volume that survives a restart, and the bucket.
  [ -s "$EXT_DIR/s3.env" ] || printf 'S3_ENDPOINT=http://%s:9000\nS3_REGION=us-east-1\nS3_BUCKET=%s\nS3_ACCESS_KEY=%s\nS3_SECRET_KEY=%s\n' "$S3_IP" "$BUCKET" "$(rand_hex 10)" "$(rand_hex 24)" > "$EXT_DIR/s3.env"
  local key secret; key="$(sed -n 's/^S3_ACCESS_KEY=//p' "$EXT_DIR/s3.env")"; secret="$(sed -n 's/^S3_SECRET_KEY=//p' "$EXT_DIR/s3.env")"
  # The secret key alone in a file (mode 0600): rusty-swarm reads a key from a file, never from the environment.
  ( umask 077; printf '%s' "$secret" > "$EXT_DIR/s3.secret" )
  if ! docker inspect "$S3_NAME" >/dev/null 2>&1; then
    docker run -d --name "$S3_NAME" --network "$EXT_NET" --ip "$S3_IP" --restart unless-stopped "${PUB_S3[@]}" \
      -e RUSTFS_ACCESS_KEY="$key" -e RUSTFS_SECRET_KEY="$secret" -v rusty-ext-s3-data:/data --tmpfs /logs:mode=1777 --tmpfs /tmp:mode=1777 \
      "$S3_IMAGE" /data >/dev/null
  else
    docker start "$S3_NAME" >/dev/null 2>&1 || true
  fi
  local i
  for i in $(seq 1 60); do
    docker exec "$PG_NAME" pg_isready -q -h 127.0.0.1 >/dev/null 2>&1 && break; sleep 2
  done
  docker exec "$PG_NAME" pg_isready -q -h 127.0.0.1 >/dev/null 2>&1 || die "PostgreSQL did not become ready (docker logs $PG_NAME)"
  docker exec "$PG_NAME" psql -U postgres -At -c "select 1 from pg_database where datname = 'rusty'" | grep -q 1 || docker exec "$PG_NAME" createdb -U postgres rusty
  for i in $(seq 1 60); do
    [ -n "$(curl -s -o /dev/null -w '%{http_code}' "http://$S3_IP:9000/" 2>/dev/null | grep -E '^[234]' || true)" ] && break; sleep 2
  done
  local code; code="$(curl -s -o /dev/null -w '%{http_code}' --aws-sigv4 "aws:amz:us-east-1:s3" --user "$key:$secret" -X PUT "http://$S3_IP:9000/$BUCKET" || true)"
  case "$code" in 200|409) ;; *) die "could not create the bucket $BUCKET (HTTP $code; docker logs $S3_NAME)" ;; esac
  attach_ext
  echo "local-cluster: the external PostgreSQL ($PG_HOST = $PG_IP, TLS only) and S3 ($S3_IP:9000, bucket $BUCKET) are up; settings in $EXT_DIR"
}

ext_status() {
  need_docker
  local n
  for n in "$PG_NAME" "$S3_NAME"; do printf '  %s: %s\n' "$n" "$(docker inspect -f '{{.State.Status}} {{.NetworkSettings.Networks}}' "$n" 2>/dev/null | cut -d' ' -f1 || echo absent)"; done
  echo "  nodes on $EXT_NET: $(docker network inspect "$EXT_NET" --format '{{range .Containers}}{{.Name}} {{end}}' 2>/dev/null)"
  echo "  files: $(ls "$EXT_DIR" 2>/dev/null | tr '\n' ' ')"
}

ext_verify() {
  need_docker
  [ -s "$EXT_DIR/ca.pem" ] && [ -s "$EXT_DIR/pg.env" ] || die "ext verify: no external services here (run: $0 ext up)"
  # shellcheck source=/dev/null
  . "$EXT_DIR/pg.env"
  local n fail=0 out
  for n in $(nodes); do
    docker exec "$n" rm -rf /tmp/ext-ca.pem; docker exec -i "$n" sh -c "cat > /tmp/ext-ca.pem" < "$EXT_DIR/ca.pem"
    # From the node, as a task would: TLS with the name checked (verify-full), and what address the database sees.
    out="$(inner "$n" run --rm -e PGPASSWORD="$PGPASSWORD" -v /tmp/ext-ca.pem:/ca.pem:ro "$PG_IMAGE" psql -At "host=$PG_HOST hostaddr=$PG_IP dbname=rusty user=postgres sslmode=verify-full sslrootcert=/ca.pem" -c "select 'tls=' || ssl || ' from ' || inet_client_addr() from pg_stat_ssl where pid = pg_backend_pid()" 2>&1 | tail -1 || true)"
    case "$out" in tls=t*) echo "ok: $n -> PostgreSQL over TLS (verify-full), seen as ${out##* from }" ;; *) echo "FAIL: $n -> PostgreSQL: $out"; fail=1 ;; esac
    out="$(inner "$n" run --rm -e PGPASSWORD="$PGPASSWORD" "$PG_IMAGE" psql -At "host=$PG_IP dbname=rusty user=postgres sslmode=disable" -c "select 1" 2>&1 | tail -1 || true)"
    case "$out" in *"no encryption"*|*"rejects"*|*"SSL"*) echo "ok: $n -> a connection without TLS is refused" ;; *) echo "FAIL: $n -> a connection without TLS was not refused: $out"; fail=1 ;; esac
    out="$(inner "$n" run --rm --entrypoint bash "$PG_IMAGE" -c "exec 3<>/dev/tcp/$S3_IP/9000 && printf 'GET / HTTP/1.0\r\n\r\n' >&3 && head -1 <&3" 2>&1 | tr -d '\r' | tail -1)"
    case "$out" in HTTP/*) echo "ok: $n -> S3 answers ($out)" ;; *) echo "FAIL: $n -> S3: $out"; fail=1 ;; esac
  done
  [ "$fail" = 0 ] || die "ext verify: a node cannot use the external services"
  echo "ext verify: every node reaches the external PostgreSQL (TLS only) and S3"
}

ext_down() {
  local purge=0; [ "${1:-}" = "--purge" ] && purge=1
  need_docker
  docker rm -f "$PG_NAME" "$S3_NAME" >/dev/null 2>&1 || true
  local n; for n in $(nodes); do docker network disconnect "$EXT_NET" "$n" >/dev/null 2>&1 || true; done
  docker network rm "$EXT_NET" >/dev/null 2>&1 || true
  if [ "$purge" = 1 ]; then docker volume rm rusty-ext-pg-data rusty-ext-pg-tls rusty-ext-s3-data >/dev/null 2>&1 || true; rm -rf "$EXT_DIR"; echo "local-cluster: the external services and their data were removed"; else echo "local-cluster: the external services were stopped and removed (the data volumes and $EXT_DIR are kept)"; fi
}

cmd_ext() {
  case "${1:-}" in
    up) ext_up ;;
    status) ext_status ;;
    verify) ext_verify ;;
    down) shift; ext_down "$@" ;;
    *) die "ext: up | status | verify | down [--purge]" ;;
  esac
}

case "${1:-}" in
  up) shift; cmd_up "$@" ;;
  status) cmd_status ;;
  ext) shift; cmd_ext "$@" ;;
  verify) shift; cmd_verify "$@" ;;
  node-stop) cmd_node stop "${2:-}" ;;
  node-start) cmd_node start "${2:-}" ;;
  docker) shift; need_docker; mgr "$@" ;;
  down) shift; cmd_down "$@" ;;
  *) sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'; [ -z "${1:-}" ] || exit 2 ;;
esac
