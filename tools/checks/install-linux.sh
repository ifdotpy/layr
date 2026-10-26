#!/bin/bash
# Run as root in the dedicated, replaceable runner VM. No transport credentials required.
set -euo pipefail
test "$(id -u)" = 0
apt-get update -qq
DEBIAN_FRONTEND=noninteractive apt-get install -y -qq btrfs-progs zstd git python3 \
    docker-registry skopeo apache2-utils bubblewrap xfsprogs e2fsprogs curl \
    ca-certificates build-essential pkg-config libssl-dev util-linux
if sysctl -n kernel.apparmor_restrict_unprivileged_userns >/dev/null 2>&1; then
    echo 'kernel.apparmor_restrict_unprivileged_userns=0' > /etc/sysctl.d/90-layr-checks.conf
    sysctl -p /etc/sysctl.d/90-layr-checks.conf
fi
id layr-checks >/dev/null 2>&1 || useradd --system --create-home --shell /usr/sbin/nologin layr-checks
seed=/usr/local/share/layr-checks
mkdir -p "$seed" /usr/local/lib/layr-checks
export CARGO_HOME="$seed/cargo" RUSTUP_HOME="$seed/rustup"
curl --fail --silent --show-error https://sh.rustup.rs -o /tmp/layr-rustup.sh
sh /tmp/layr-rustup.sh -y --no-modify-path --profile minimal
export PATH="$CARGO_HOME/bin:/usr/local/go/bin:$PATH"
cargo fetch --locked
# Go's official release metadata supplies an architecture-matched source-build toolchain.
python3 - <<'PY'
import hashlib,json,pathlib,platform,urllib.request,tarfile
releases=json.load(urllib.request.urlopen('https://go.dev/dl/?mode=json'))
arch={'aarch64':'arm64','x86_64':'amd64'}[platform.machine()]
item=next(f for f in releases[0]['files'] if f['os']=='linux' and f['arch']==arch and f['kind']=='archive')
data=urllib.request.urlopen('https://go.dev/dl/'+item['filename']).read()
assert hashlib.sha256(data).hexdigest()==item['sha256']
p=pathlib.Path('/tmp/layr-go.tar.gz'); p.write_bytes(data)
with tarfile.open(p) as t: t.extractall('/usr/local',filter='data')
p.unlink()
PY
CGO_ENABLED=0 GOBIN=/usr/local/lib/layr-checks go install github.com/minio/minio@RELEASE.2025-10-15T17-29-55Z
# Helm's pinned official release archive.
arch=$(dpkg --print-architecture)
curl -fsSLo /tmp/layr-helm.tar.gz "https://get.helm.sh/helm-v3.19.0-linux-$arch.tar.gz"
curl -fsSLo /tmp/layr-helm.sha256 "https://get.helm.sh/helm-v3.19.0-linux-$arch.tar.gz.sha256sum"
(cd /tmp && sed "s@helm-v3.19.0-linux-$arch.tar.gz@layr-helm.tar.gz@" layr-helm.sha256 | sha256sum -c -)
tar -xzf /tmp/layr-helm.tar.gz -C /tmp
install -m 755 "/tmp/linux-$arch/helm" /usr/local/bin/helm
chmod -R a+rX "$seed"
cargo build --locked --offline
install -m 755 target/debug/layr /usr/local/bin/layr
echo 'Runner tools installed. Provision the project and install the services next.'
