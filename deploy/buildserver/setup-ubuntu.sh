#!/usr/bin/env bash
# One-time setup of the Rusty BUILD SERVER on a fresh Ubuntu 24.04 LTS Server VM.
#   sudo bash setup-ubuntu.sh --ssh-from 10.77.0.0/24 --pubkey-file /tmp/id_ed25519.pub
# Idempotent: safe to re-run. See docs/buildserver.md.
#
# SECURITY: the build user is in the `docker` group, which is ROOT-EQUIVALENT on this VM.
# The VM must be isolated and disposable: no secrets, no real credentials, NAT-only network.
set -euo pipefail

# ---- pinned versions (edit deliberately; re-verify after bumping) -----------------------------
CARGO_DENY_VERSION="${CARGO_DENY_VERSION:-0.20.0}"
CARGO_AUDIT_VERSION="${CARGO_AUDIT_VERSION:-0.22.0}"
CARGO_FUZZ_VERSION="${CARGO_FUZZ_VERSION:-0.13.1}"
NIGHTLY_TOOLCHAIN="${NIGHTLY_TOOLCHAIN:-nightly-2026-11-02}"   # for cargo-fuzz only (nightly-2026-11-01 fails to compile sqlx-core: E0391)
RUSTUP_INIT_URL="https://static.rust-lang.org/rustup/dist/x86_64-unknown-linux-gnu/rustup-init"
RUSTUP_INIT_SHA256="${RUSTUP_INIT_SHA256:-}"   # optional hard pin; else the .sha256 from static.rust-lang.org
DOCKER_KEY_FPR="9DC858229FC7DD38854AE2D88D81803C0EBFCD88"      # Docker Inc. apt signing key
DOCKER_KEY_URL="https://download.docker.com/linux/ubuntu/gpg"
DOCKER_REPO="https://download.docker.com/linux/ubuntu"
BUILD_USER="rustybuild"
RUSTY_ROOT="/srv/rusty"
SWAP_SIZE_GB=8
# -----------------------------------------------------------------------------------------------

SSH_FROM="" PUBKEY="" PUBKEY_FILE=""
usage() { echo "usage: sudo bash $0 --ssh-from <CIDR> (--pubkey-file FILE | --pubkey 'ssh-ed25519 AAAA...')" >&2; exit 2; }
while [ $# -gt 0 ]; do
  case "$1" in
    --ssh-from) SSH_FROM="${2:-}"; shift 2 ;;
    --pubkey-file) PUBKEY_FILE="${2:-}"; shift 2 ;;
    --pubkey) PUBKEY="${2:-}"; shift 2 ;;
    -h|--help) usage ;;
    *) echo "unknown argument: $1" >&2; usage ;;
  esac
done
[ "$(id -u)" -eq 0 ] || { echo "error: run as root (sudo)" >&2; exit 1; }
[[ "$SSH_FROM" =~ ^[0-9]{1,3}(\.[0-9]{1,3}){3}/[0-9]{1,2}$ ]] || { echo "error: --ssh-from must be an IPv4 CIDR (the NAT subnet), e.g. 10.77.0.0/24" >&2; usage; }
[ -z "$PUBKEY_FILE" ] || PUBKEY="$(cat "$PUBKEY_FILE")"
if [ -z "$PUBKEY" ] && [ ! -s "/home/$BUILD_USER/.ssh/authorized_keys" ]; then
  echo "error: no public key given and none installed for $BUILD_USER" >&2; usage
fi
if [ -n "$PUBKEY" ] && ! [[ "$PUBKEY" =~ ^(ssh-ed25519|ecdsa-sha2-nistp256|sk-ssh-ed25519@openssh.com)\ [A-Za-z0-9+/=]+(\ .*)?$ ]]; then
  echo "error: public key must be a single-line ed25519/ecdsa key" >&2; exit 1
fi
# shellcheck disable=SC1091
. /etc/os-release
if [ "${ID:-}" != ubuntu ] || [ "${VERSION_ID:-}" != "24.04" ]; then
  echo "error: this script targets Ubuntu 24.04 LTS (found ${PRETTY_NAME:-unknown})" >&2; exit 1
fi
[ "$(uname -m)" = x86_64 ] || { echo "error: x86_64 only" >&2; exit 1; }

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
step() { printf '\n==== %s ====\n' "$*"; }
export DEBIAN_FRONTEND=noninteractive

step "apt packages"
apt-get update -y
apt-get install -y --no-install-recommends \
  build-essential git curl ca-certificates gnupg pkg-config musl-tools openssl libssl-dev \
  netcat-openbsd swaks shellcheck jq rsync ufw unattended-upgrades openssh-server util-linux \
  xz-utils

step "Docker Engine (official apt repo, key fingerprint verified)"
install -m 0755 -d /etc/apt/keyrings
if [ ! -s /etc/apt/keyrings/docker.gpg ]; then
  tmpk="$(mktemp)"; curl -fsSL --proto '=https' --tlsv1.2 "$DOCKER_KEY_URL" -o "$tmpk"
  got="$(gpg --show-keys --with-colons "$tmpk" | awk -F: '$1=="fpr"{print $10; exit}')"
  if [ "$got" != "$DOCKER_KEY_FPR" ]; then
    echo "error: Docker key fingerprint mismatch ($got)" >&2; rm -f "$tmpk"; exit 1
  fi
  gpg --dearmor < "$tmpk" > /etc/apt/keyrings/docker.gpg; rm -f "$tmpk"
  chmod 0644 /etc/apt/keyrings/docker.gpg
fi
echo "deb [arch=amd64 signed-by=/etc/apt/keyrings/docker.gpg] $DOCKER_REPO noble stable" > /etc/apt/sources.list.d/docker.list
apt-get update -y
apt-get install -y docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin
systemctl enable --now docker

step "build user $BUILD_USER (no password, key login only)"
if ! id "$BUILD_USER" >/dev/null 2>&1; then
  adduser --disabled-password --gecos "Rusty build user" "$BUILD_USER"
fi
usermod -aG docker "$BUILD_USER"
passwd -l "$BUILD_USER" >/dev/null 2>&1 || true
install -d -m 0700 -o "$BUILD_USER" -g "$BUILD_USER" "/home/$BUILD_USER/.ssh"
if [ -n "$PUBKEY" ]; then
  ak="/home/$BUILD_USER/.ssh/authorized_keys"; touch "$ak"
  grep -qxF "$PUBKEY" "$ak" || echo "$PUBKEY" >> "$ak"
  chown "$BUILD_USER:$BUILD_USER" "$ak"; chmod 0600 "$ak"
fi
# user services (systemd-run --user for detached CI) must survive logout
loginctl enable-linger "$BUILD_USER"

step "directories under $RUSTY_ROOT"
install -d -m 0755 -o "$BUILD_USER" -g "$BUILD_USER" "$RUSTY_ROOT"
for d in work target logs; do install -d -m 0755 -o "$BUILD_USER" -g "$BUILD_USER" "$RUSTY_ROOT/$d"; done

step "sysctl and limits"
cat > /etc/sysctl.d/90-rusty-build.conf <<'EOS'
fs.inotify.max_user_watches = 524288
fs.inotify.max_user_instances = 1024
fs.file-max = 2097152
vm.swappiness = 10
EOS
sysctl --system >/dev/null
cat > /etc/security/limits.d/90-rusty-build.conf <<EOS
$BUILD_USER soft nofile 1048576
$BUILD_USER hard nofile 1048576
EOS
install -d /etc/systemd/system.conf.d
printf '[Manager]\nDefaultLimitNOFILE=1048576\n' > /etc/systemd/system.conf.d/90-rusty-nofile.conf

step "swap"
if [ "$(swapon --show --noheadings | wc -l)" -eq 0 ]; then
  if [ ! -f /swapfile ]; then
    fallocate -l "${SWAP_SIZE_GB}G" /swapfile 2>/dev/null || dd if=/dev/zero of=/swapfile bs=1M count=$((SWAP_SIZE_GB * 1024)) status=none
    chmod 600 /swapfile; mkswap /swapfile >/dev/null
  fi
  swapon /swapfile
  grep -q '^/swapfile ' /etc/fstab || echo '/swapfile none swap sw 0 0' >> /etc/fstab
else
  echo "swap already present"
fi

step "unattended-upgrades"
printf 'APT::Periodic::Update-Package-Lists "1";\nAPT::Periodic::Unattended-Upgrade "1";\n' > /etc/apt/apt.conf.d/20auto-upgrades
systemctl enable --now unattended-upgrades

step "journald size cap"
install -d /etc/systemd/journald.conf.d
printf '[Journal]\nSystemMaxUse=500M\nMaxRetentionSec=2week\n' > /etc/systemd/journald.conf.d/90-rusty.conf
systemctl restart systemd-journald

step "sshd hardening drop-in"
# ALLOW_ROOT_LOGIN=1 keeps key-only root login (for an owner-managed, isolated build VM); default is build user only.
if [ "${ALLOW_ROOT_LOGIN:-0}" = 1 ]; then SSHD_PERMIT_ROOT="prohibit-password"; SSHD_ALLOW_USERS="$BUILD_USER root"; else SSHD_PERMIT_ROOT="no"; SSHD_ALLOW_USERS="$BUILD_USER"; fi
cat > /etc/ssh/sshd_config.d/00-rusty-hardening.conf <<EOS
PermitRootLogin $SSHD_PERMIT_ROOT
PasswordAuthentication no
KbdInteractiveAuthentication no
PubkeyAuthentication yes
AllowUsers $SSHD_ALLOW_USERS
X11Forwarding no
MaxAuthTries 3
ClientAliveInterval 30
ClientAliveCountMax 6
EOS
sshd -t
systemctl reload ssh

step "UFW: SSH only from $SSH_FROM"
ufw --force reset >/dev/null
ufw default deny incoming
ufw default allow outgoing
ufw allow from "$SSH_FROM" to any port 22 proto tcp
ufw --force enable
# NOTE: Docker-published ports bypass UFW. The repo's test containers publish only on 127.0.0.1.

step "rustup + toolchain (as $BUILD_USER)"
toolchain="$(sed -n 's/^channel *= *"\(.*\)"/\1/p' "$here/../../rust-toolchain.toml" 2>/dev/null | head -1)"
if [ -z "$toolchain" ]; then
  echo "error: cannot read channel from rust-toolchain.toml (copy the repo checkout to the VM and run the script from it)" >&2; exit 1
fi
echo "toolchain from rust-toolchain.toml: $toolchain"
as_build() { sudo -u "$BUILD_USER" -H env "PATH=/home/$BUILD_USER/.cargo/bin:/usr/local/bin:/usr/bin:/bin" bash -c "$1"; }
if [ ! -x "/home/$BUILD_USER/.cargo/bin/rustup" ]; then
  tmpd="$(mktemp -d)"; chmod 755 "$tmpd"
  curl -fsSL --proto '=https' --tlsv1.2 "$RUSTUP_INIT_URL" -o "$tmpd/rustup-init"
  if [ -n "$RUSTUP_INIT_SHA256" ]; then
    want="$RUSTUP_INIT_SHA256"
  else
    want="$(curl -fsSL --proto '=https' --tlsv1.2 "$RUSTUP_INIT_URL.sha256" | awk '{print $1}')"
  fi
  have="$(sha256sum "$tmpd/rustup-init" | awk '{print $1}')"
  if ! [[ "$want" =~ ^[0-9a-f]{64}$ ]] || [ "$want" != "$have" ]; then
    echo "error: rustup-init checksum mismatch (want $want have $have)" >&2; exit 1
  fi
  chmod 755 "$tmpd/rustup-init"
  sudo -u "$BUILD_USER" -H "$tmpd/rustup-init" -y --no-modify-path --profile minimal \
    --default-toolchain "$toolchain" -c clippy -c rustfmt -t x86_64-unknown-linux-musl
  rm -rf "$tmpd"
fi
as_build "rustup toolchain install '$toolchain' --profile minimal -c clippy -c rustfmt -t x86_64-unknown-linux-musl && rustup default '$toolchain'"
as_build "rustup toolchain install '$NIGHTLY_TOOLCHAIN' --profile minimal"
# scripts/ci.sh and cargo-fuzz call plain '+nightly': alias it to the pinned date
as_build "rustup toolchain link nightly \"\$(rustup run '$NIGHTLY_TOOLCHAIN' rustc --print sysroot)\" 2>/dev/null || true"
as_build "grep -q cargo/env ~/.profile 2>/dev/null || echo '. \"\$HOME/.cargo/env\"' >> ~/.profile"

step "cargo tools (--locked, pinned)"
as_build "cargo install --locked cargo-deny --version '$CARGO_DENY_VERSION'"
as_build "cargo install --locked cargo-audit --version '$CARGO_AUDIT_VERSION'"
as_build "cargo install --locked cargo-fuzz --version '$CARGO_FUZZ_VERSION'"

step "verification"
as_build "rustc --version; cargo --version; rustup toolchain list; rustup target list --installed"
as_build "cargo deny --version; cargo audit --version; cargo fuzz --version"
docker --version
docker compose version
swaks --version | sed -n 1,2p
shellcheck --version | sed -n 1,2p
jq --version
rsync --version | sed -n 1p
tdir="$(mktemp -d)"; chmod 755 "$tdir"; chown "$BUILD_USER" "$tdir"
as_build "cd '$tdir' && cargo new --quiet --bin hello && cd hello && cargo build --quiet && ./target/debug/hello && cargo build --quiet --release --target x86_64-unknown-linux-musl"
rm -rf "$tdir"
lock="$here/../images.lock"
if [ -r "$lock" ]; then
  img="$(awk '$1 ~ /^haproxy:/ {print $2}' "$lock" | head -1)"
  [ -n "$img" ] || { echo "error: haproxy digest not found in images.lock" >&2; exit 1; }
  echo "docker check with pinned image $img"
  sudo -u "$BUILD_USER" -H docker run --rm "$img" haproxy -v | sed -n 1p
else
  echo "WARNING: deploy/images.lock not found next to this script; docker run check skipped" >&2
fi

step "summary"
echo "Build user      : $BUILD_USER (key login only; member of docker = root-equivalent)"
echo "Directories     : $RUSTY_ROOT/{work,target,logs}"
echo "SSH allowed from: $SSH_FROM (UFW)"
echo "VM address(es)  : $(hostname -I)"
echo "SSH host key fingerprint (compare with 'scripts/remote-sync.sh --pin-host' on Windows):"
ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub
echo "Next: take a VM snapshot named 'clean-build-server', then fill .rusty-remote.env (docs/buildserver.md)."
echo "SETUP OK"
