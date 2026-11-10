# Build server (Ubuntu VM)

The WSL VM crashes under load, so builds, tests and the full CI gate can run on a dedicated Ubuntu
Server VM instead. You edit on Windows as before; the `scripts/remote-*.sh` helpers (run from
Git Bash) sync the worktree to the VM over SSH and run cargo / `scripts/ci.sh` there. The repo
scripts run unchanged on the server. `scripts/wsl-*.sh` stay as the fallback.

This VM is the **build server**. It is not one of the two mail-instance test VMs of the master
plan U8b (`alpha`/`beta`, VMnet10 `10.77.0.0/24`, .10 DNS, .11 alpha, .12 beta). Keep them as
separate VMs; the build server only builds and runs unit/integration tests (and throw-away test
Postgres containers).

## 1. Create the VM (VMware Workstation 26)

Recommended size: **8 vCPU, 16 GB RAM, 150 GB disk** (target dirs and Docker layers are large;
use a single growable virtual disk).

1. New Virtual Machine, Typical, installer disc image = the Ubuntu Server 24.04 LTS ISO.
2. Guest OS Linux, Ubuntu 64-bit. Hardware as above; keep 3D acceleration off.
3. Network adapter: **NAT**. Either:
   - the default VMnet8 (simple; note its subnet in Virtual Network Editor, typically
     `192.168.x.0/24`), or
   - the dedicated VMnet10 `10.77.0.0/24` described in the master plan U8b (creating it needs
     admin rights in the Virtual Network Editor; do not change VMnet8). Suggested address for
     the build server: `10.77.0.20`.
4. Installer: language/keyboard, **Ubuntu Server (minimal)**, network as below, no proxy, default
   mirror, use the whole disk (LVM is fine), create a user (for administration only, e.g.
   `admin`), tick **Install OpenSSH server**, no extra snaps.
5. Static IP (recommended). In the installer network screen choose "Edit IPv4 > Manual", or after
   install edit `/etc/netplan/50-cloud-init.yaml` and run `sudo netplan apply`:

   ```yaml
   network:
     version: 2
     ethernets:
       ens33:                      # check the name with: ip -br link
         dhcp4: false
         addresses: [10.77.0.20/24]
         routes:
           - to: default
             via: 10.77.0.2        # the VMnet NAT gateway (VMnet8 default: x.x.x.2)
         nameservers:
           addresses: [10.77.0.2]
   ```

6. **Take a snapshot** named `fresh-os` before running anything else.

## 2. SSH key and first setup

On Windows (Git Bash or PowerShell):

    ssh-keygen -t ed25519 -f "$HOME/.ssh/rusty_build_ed25519" -C rusty-build

Copy the public key and the repo checkout to the VM (the setup script reads
`rust-toolchain.toml` and `deploy/images.lock` from the checkout), using the admin account:

    scp "$HOME/.ssh/rusty_build_ed25519.pub" admin@10.77.0.20:/tmp/
    ssh admin@10.77.0.20 mkdir -p /home/admin/rusty-setup
    scp -r deploy rust-toolchain.toml admin@10.77.0.20:/home/admin/rusty-setup/

(Only `deploy/buildserver`, `deploy/images.lock` and `rust-toolchain.toml` are needed; keep the
same relative layout.) Then on the VM:

    cd ~/rusty-setup
    sudo bash deploy/buildserver/setup-ubuntu.sh \
        --ssh-from 10.77.0.0/24 --pubkey-file /tmp/rusty_build_ed25519.pub

`--ssh-from` is the NAT subnet the Windows host reaches the VM from (UFW then allows SSH from
that subnet only). Check the subnet before running it: a wrong value locks you out of SSH (use the
VM console to fix it with `sudo ufw allow from <cidr> to any port 22`).

The script is idempotent and takes a while (it compiles cargo-deny, cargo-audit and cargo-fuzz).
It installs the build packages, Docker Engine from Docker's apt repo (key fingerprint verified),
rustup with the exact toolchain from `rust-toolchain.toml`, the musl target, a pinned nightly for
cargo-fuzz and the pinned cargo tools (versions in the variable block at the top of the script).
It creates the `rustybuild` user (no password, key login only), `/srv/rusty/{work,target,logs}`,
sysctl/ulimit values, an 8 GB swap file if there is none, unattended upgrades, UFW, an sshd
hardening drop-in (key-only, no root login) and a journald cap. It ends by printing versions,
building a tiny crate, running a pinned-digest container from `deploy/images.lock` and printing
the SSH host key fingerprint, followed by `SETUP OK`.

Take a second snapshot named `clean-build-server`.

## 3. Configure the Windows side

    cp .rusty-remote.env.example .rusty-remote.env      # untracked; edit it
    bash scripts/remote-sync.sh --pin-host              # compare the fingerprint with the setup output!
    bash scripts/remote-sync.sh --check

`.rusty-remote.env` values: `BUILD_HOST` (VM IP), `BUILD_USER` (`rustybuild`), `BUILD_KEY` (path
to the private key, forward slashes), optional `BUILD_PORT`. The pinned host key is stored next to
the file in `.rusty-remote.known_hosts` (also untracked) and SSH uses `StrictHostKeyChecking=yes`,
`BatchMode=yes` (no password prompts), keepalives and a connect timeout. Both files exist per
checkout/worktree; copy them to other worktrees or point `RUSTY_REMOTE_ENV` at one shared file.

Verify, then start the first CI run:

    bash scripts/remote-cargo.sh --version
    bash scripts/remote-ci.sh --detach

## 4. Daily use (humans and agents)

One command for the full gate, whichever machine is configured:

    bash scripts/ci-run.sh --detach      # remote when .rusty-remote.env exists, else WSL (no flags)
    bash scripts/remote-ci.sh --status   # STATE=running|finished|died|none; exit code = run result
    bash scripts/remote-ci.sh --tail 60  # last log lines (--follow keeps following)
    bash scripts/remote-ci.sh --wait     # block until done; exit code = CI exit code

Cargo on the server: `bash scripts/remote-cargo.sh test -p rusty-store`,
`bash scripts/remote-cargo.sh clippy --workspace --all-targets -- -D warnings`.

- The worktree (including uncommitted changes; excluding `target/`, `.git`, fuzz artifacts and
  git-ignored files) is synced to `/srv/rusty/work/<worktree-name>/` with rsync, or with tar over
  SSH when rsync is not installed on Windows (the helper says so). `--delete` applies only inside
  that directory; paths outside `/srv/rusty/{work,target}/<name>` are refused.
- Target dir: `/srv/rusty/target/<worktree-name>` (one per worktree). Logs:
  `/srv/rusty/logs/<worktree>-<timestamp>-<cargo|ci>.log`; the path is printed on every run.
- Locks: two runs of the same worktree never overlap, and only one `scripts/ci.sh` runs on the
  server at a time. A busy run exits 75 and names the holder; `BUILD_LOCK_WAIT=<seconds>` waits
  instead.
- `--detach` starts CI under `systemd-run --user` (nohup fallback), so a dead Windows session or
  agent does not kill the run. A foreground run that loses its SSH connection does die; prefer
  `--detach` for CI. Agents: start with `--detach`, then poll `--status`, read `--tail`, finish
  with `--wait`. Do not start a second CI for the same checkout.
- Postgres tests need no extra setup: `scripts/ci.sh` creates a per-run container id exactly as
  on WSL (`scripts/pg-lib.sh`), and the user is in the `docker` group.

## 5. Security notes

- The `rustybuild` user is in the `docker` group, which is **root-equivalent** on the VM. Treat the
  VM as untrusted-by-design: isolated NAT network, no credentials, no personal data, no
  production keys, no cloud tokens. SSH in is from the NAT subnet only, key-only.
- The VM runs code from the repo and its dependencies (build scripts, tests); that is the reason
  it exists apart from the Windows host. Never mount Windows shares into it.
- Snapshots: `fresh-os` (before setup), `clean-build-server` (after). Revert to `clean-build-server`
  whenever the VM misbehaves or after running something you do not trust.
- Rebuilding it disposably: delete the VM, create a new one (section 1), re-run setup, then
  delete `.rusty-remote.known_hosts` and run `--pin-host` again (new host key; the helper refuses
  a changed key on purpose).
- Docker-published ports bypass UFW; the repo's test containers bind to 127.0.0.1 only.
- Pinned versions live in the variable block of `deploy/buildserver/setup-ubuntu.sh`; bump them
  deliberately.

## 6. Troubleshooting

| Symptom | Fix |
|---|---|
| `no pinned host key` | `bash scripts/remote-sync.sh --pin-host` |
| `Host key verification failed` | VM rebuilt: delete `.rusty-remote.known_hosts`, pin again after checking the fingerprint |
| `Permission denied (publickey)` | wrong `BUILD_KEY`, or the public key is not in `/home/rustybuild/.ssh/authorized_keys` |
| connection timeout | wrong IP/subnet; UFW `--ssh-from` does not include your host; check `ip -br a` on the VM console |
| `busy ... held by` (exit 75) | another run holds the lock; `--status`/`--tail`, or `BUILD_LOCK_WAIT=600` |
| `STATE=died` | the run was killed (VM reboot, OOM); read `--tail`, start again; if the target dir looks corrupt run `bash scripts/remote-cargo.sh clean` |
| `rsync not available locally` | informational; the tar fallback is used. Installing rsync in Git Bash is optional |
| Docker permission denied | log in again after setup (group change), or `sudo usermod -aG docker rustybuild` |
| disk full | `docker system prune -af`, remove old `/srv/rusty/target/*` and `/srv/rusty/logs/*` |
| CI log shows `DEGRADED` | Docker or cargo-fuzz/nightly missing; re-run setup |
