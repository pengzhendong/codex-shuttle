#!/usr/bin/env bash
set -euo pipefail

: "${CODEX_VERSION:?CODEX_VERSION is required}"

report_error() {
  status=$?
  line=$1
  command=$2
  command=${command//'%'/'%25'}
  command=${command//$'\r'/'%0D'}
  command=${command//$'\n'/'%0A'}
  echo "::error title=Remote E2E failure::line $line: $command (exit $status)"
}
trap 'report_error "$LINENO" "$BASH_COMMAND"' ERR

kernel=$(uname -s)
arch=$(uname -m)
case "$kernel:$arch" in
  Linux:x86_64)
    target=x86_64-unknown-linux-musl
    ;;
  Linux:aarch64|Linux:arm64)
    target=aarch64-unknown-linux-musl
    ;;
  Darwin:arm64|Darwin:aarch64)
    target=aarch64-apple-darwin
    ;;
  *)
    echo "unsupported CI remote target $kernel $arch" >&2
    exit 1
    ;;
esac

package_name="codex-package-${target}.tar.gz"
release_base="https://github.com/openai/codex/releases/download/rust-v${CODEX_VERSION}"
cache_dir=${CXS_CI_CACHE_DIR:-"$RUNNER_TEMP/codex-shuttle-package"}
work_dir="$RUNNER_TEMP/codex-shuttle-e2e"
package="$cache_dir/$package_name"
manifest="$cache_dir/codex-package_SHA256SUMS"
runtime="$work_dir/runtime"
ssh_dir="$work_dir/ssh"
profile=ci-remote
port=42222

mkdir -p "$cache_dir" "$runtime" "$ssh_dir" "$HOME/.ssh"
chmod 700 "$HOME/.ssh" "$ssh_dir"
curl --fail --location --retry 3 --silent --show-error \
  --output "$manifest" "$release_base/codex-package_SHA256SUMS"
expected=$(awk -v wanted="$package_name" '$2 == wanted || $2 == "*" wanted { print $1; exit }' "$manifest")
test -n "$expected"
if test -f "$package"; then
  if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$package" | awk '{print $1}')
  else
    actual=$(shasum -a 256 "$package" | awk '{print $1}')
  fi
  test "$actual" = "$expected" || rm -f "$package"
fi
if test ! -f "$package"; then
  curl --fail --location --retry 3 --silent --show-error \
    --output "$package" "$release_base/$package_name"
fi
if command -v sha256sum >/dev/null 2>&1; then
  actual=$(sha256sum "$package" | awk '{print $1}')
else
  actual=$(shasum -a 256 "$package" | awk '{print $1}')
fi
test "$actual" = "$expected"
tar -xzf "$package" -C "$runtime"
test -x "$runtime/bin/codex"
test -x "$runtime/bin/codex-code-mode-host"
test -x "$runtime/codex-path/rg"
test "$("$runtime/bin/codex" --version)" = "codex-cli $CODEX_VERSION"
"$runtime/bin/codex" exec-server --help >/dev/null

cargo build --locked --release -p cxs-cli -p cxs-shim
cxs="$GITHUB_WORKSPACE/target/release/cxs"
shim="$GITHUB_WORKSPACE/target/release/cxs-shim"
test -x "$cxs"
test -x "$shim"

ssh-keygen -q -t ed25519 -N '' -f "$ssh_dir/host_key"
ssh-keygen -q -t ed25519 -N '' -f "$ssh_dir/client_key"
cp "$ssh_dir/client_key.pub" "$ssh_dir/authorized_keys"
chmod 600 "$ssh_dir/authorized_keys"
sudo chown root "$ssh_dir/host_key"
if test "$kernel" = Linux; then
  # Ubuntu's AppArmor policy grants user-namespace access to the system
  # bubblewrap path. Codex's vendored copy is outside that policy and fails
  # while configuring the sandbox loopback interface on hosted runners.
  sudo apt-get update
  sudo apt-get install --yes apparmor-profiles apparmor-utils bubblewrap
  apparmor_profile=/usr/share/apparmor/extra-profiles/bwrap-userns-restrict
  if test -f "$apparmor_profile"; then
    sudo install -m 0644 "$apparmor_profile" /etc/apparmor.d/bwrap-userns-restrict
    sudo apparmor_parser -r /etc/apparmor.d/bwrap-userns-restrict
  fi
  if sysctl kernel.apparmor_restrict_unprivileged_userns >/dev/null 2>&1; then
    sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0
  fi
  bwrap --ro-bind / / --dev /dev --proc /proc --unshare-user --unshare-net /bin/true
  sudo mkdir -p /run/sshd
  # GitHub's Linux runner account is locked even though sudo is available.
  # Public-key auth is still disabled for locked accounts, so clear the
  # ephemeral password while keeping password authentication disabled below.
  sudo passwd -d "$USER"
fi
cat >"$ssh_dir/sshd_config" <<EOF
Port $port
ListenAddress 127.0.0.1
HostKey $ssh_dir/host_key
PidFile $ssh_dir/sshd.pid
AuthorizedKeysFile $ssh_dir/authorized_keys
PasswordAuthentication no
KbdInteractiveAuthentication no
UsePAM no
PermitRootLogin no
StrictModes no
AllowUsers $USER
LogLevel VERBOSE
Subsystem sftp internal-sftp
EOF

sshd_log="$ssh_dir/sshd.log"
sudo /usr/sbin/sshd -D -e -f "$ssh_dir/sshd_config" >"$sshd_log" 2>&1 &
sshd_launcher=$!
cleanup() {
  exit_code=$?
  "$cxs" down "$profile" >/dev/null 2>&1 || true
  if test "$exit_code" -ne 0; then
    echo "bridge diagnostic:" >&2
    sed -n '1,240p' "$HOME/.local/state/codex-shuttle/profiles/$profile/bridge.log" >&2 || true
  fi
  "$cxs" remove "$profile" --remote --purge >/dev/null 2>&1 || true
  if test -f "$ssh_dir/sshd.pid"; then
    sudo kill "$(cat "$ssh_dir/sshd.pid")" >/dev/null 2>&1 || true
  fi
  kill "$sshd_launcher" >/dev/null 2>&1 || true
  if test "$exit_code" -ne 0; then
    echo "sshd diagnostic:" >&2
    sed -n '1,200p' "$sshd_log" >&2 || true
  fi
  exit "$exit_code"
}
trap cleanup EXIT

ssh_options=(
  -F /dev/null
  -i "$ssh_dir/client_key"
  -p "$port"
  -o BatchMode=yes
  -o IdentitiesOnly=yes
  -o StrictHostKeyChecking=no
  -o UserKnownHostsFile=/dev/null
  -o LogLevel=ERROR
)
for _ in $(seq 1 50); do
  if ssh "${ssh_options[@]}" "$USER@127.0.0.1" true 2>/dev/null; then
    break
  fi
  sleep 0.1
done
ssh "${ssh_options[@]}" "$USER@127.0.0.1" true

cat >>"$HOME/.ssh/config" <<EOF

Host cxs-ci-source
  HostName 127.0.0.1
  User $USER
  Port $port
  IdentityFile $ssh_dir/client_key
  IdentitiesOnly yes
  StrictHostKeyChecking no
  UserKnownHostsFile /dev/null
  LogLevel ERROR
EOF
chmod 600 "$HOME/.ssh/config"

cp "$package" "/tmp/cxs-${profile}-codex.tar.gz"
export CXS_CODEX_PATH="$runtime/bin/codex"
"$cxs" add cxs-ci-source --name "$profile"
"$cxs" install "$profile" --shim "$shim"
if ! doctor_output=$("$cxs" doctor "$profile" --json 2>&1); then
  printf '%s\n' "$doctor_output"
  doctor_detail=$(printf '%s\n' "$doctor_output" | tail -n 24)
  doctor_detail=${doctor_detail//'%'/'%25'}
  doctor_detail=${doctor_detail//$'\r'/'%0D'}
  doctor_detail=${doctor_detail//$'\n'/'%0A'}
  echo "::error title=Remote doctor failure::$doctor_detail"
  exit 1
fi
printf '%s\n' "$doctor_output" | tee "$work_dir/doctor.json"
grep -F '"ready": true' "$work_dir/doctor.json"
"$cxs" status "$profile" | tee "$work_dir/status.txt"
grep -F 'Usable in App: yes' "$work_dir/status.txt"
ssh -o BatchMode=yes "cxs-$profile" '$HOME/.local/bin/codex --version' |
  grep -F "codex-cli $CODEX_VERSION"

"$cxs" down "$profile"
"$cxs" remove "$profile" --remote --purge
trap - EXIT
if test -f "$ssh_dir/sshd.pid"; then
  sudo kill "$(cat "$ssh_dir/sshd.pid")" >/dev/null 2>&1 || true
fi
kill "$sshd_launcher" >/dev/null 2>&1 || true
