#!/usr/bin/env bash
# A throw-away Docker Swarm cluster on one machine, for testing the swarm deployment (docs/design-swarm.md, section 3b): the nodes are privileged `docker:dind` containers on one Docker network.
# Run it where Docker runs (the WSL2 Ubuntu of the owner's PC, or any Linux with Docker).
#
#   ./local-cluster.sh up [--workers N]        create the Manager `rusty-m1` and N workers (default 2) `rusty-w1`..., join them, set the labels
#   ./local-cluster.sh status                  the nodes, their labels and the ports the Manager publishes to this machine
#   ./local-cluster.sh verify [--encrypted]    prove the data path: a task on every node, `tasks.<service>` lists them all, a ping crosses nodes (an encrypted overlay with --encrypted)
#   ./local-cluster.sh node-stop NAME          stop a node (a node loss) / node-start NAME starts it again
#   ./local-cluster.sh docker [ARGS...]        run `docker ARGS` against the Manager (for example: ./local-cluster.sh docker node ls)
#   ./local-cluster.sh down [--purge]          remove the nodes and the network (--purge also removes their volumes)
#
# The Manager publishes the edge's ports to this machine on high ports (10025 for 25, 10080 for 80, 10143, 10443, 10465, 10587, 10993): the edge is a host-mode task on the Manager, its ports exist inside
# `rusty-m1`. Labels: `outbound=true` on the first worker and `av=true` on the second (the first when there is only one), standing in for the real roles.
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
  local workers=2
  while [ $# -gt 0 ]; do
    case "$1" in
      --workers) workers="${2:-}"; shift 2 ;;
      *) die "up: unknown argument $1" ;;
    esac
  done
  [[ "$workers" =~ ^[1-9]$ ]] || die "--workers is a number from 1 to 9"
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
    if [ "$(inner "$w" info --format '{{.Swarm.LocalNodeState}}')" != active ]; then
      inner "$w" swarm join --token "$token" "$ip:2377" >/dev/null
      echo "local-cluster: $w joined"
    fi
  done
  mgr node update --label-add outbound=true rusty-w1 >/dev/null
  if [ "$workers" -ge 2 ]; then mgr node update --label-add av=true rusty-w2 >/dev/null; else mgr node update --label-add av=true rusty-w1 >/dev/null; fi
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

case "${1:-}" in
  up) shift; cmd_up "$@" ;;
  status) cmd_status ;;
  verify) shift; cmd_verify "$@" ;;
  node-stop) cmd_node stop "${2:-}" ;;
  node-start) cmd_node start "${2:-}" ;;
  docker) shift; need_docker; mgr "$@" ;;
  down) shift; cmd_down "$@" ;;
  *) sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'; [ -z "${1:-}" ] || exit 2 ;;
esac
