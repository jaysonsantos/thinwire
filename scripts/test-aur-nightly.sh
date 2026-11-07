#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
script="${script_dir}/aur-nightly.sh"
root=$(mktemp -d "${TMPDIR:-/tmp}/aur-nightly-test.XXXXXX")
original_path=$PATH
bin="${root}/bin"
test_key='TEST-AUR-KEY-NOT-SECRET'
pinned_fp='SHA256:RFzBCUItH9LZS0cKB5UE6ceAYhBD5C8GeOBip8Z11+4'
pushed_line='Pushed thinwire-nightly-bin 0.1.0.r282.gbe00026 to the AUR.'
fingerprint_error='::error::The host key of aur.archlinux.org does not match the fingerprint on the AUR home page.'

cleanup() {
  rm -rf -- "$root"
}
trap cleanup EXIT

pass() {
  printf 'PASS: %s\n' "$1"
}

fail() {
  printf 'FAIL: %s\n' "$1" >&2
  exit 1
}

install_mocks() {
  mkdir -p "$bin" "${root}/tmp"

  cat >"${bin}/curl" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
url=
for url in "$@"; do
  :
done
case "$url" in
  */nightly/VERSION)
    printf '%s\n' '0.1.0.r282.gbe00026'
    ;;
  */nightly/SHA256SUMS)
    printf '%s\n' '0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  thinwire-linux-x86_64-0.1.0.r282.gbe00026.tar.gz'
    ;;
  *)
    printf 'curl: unexpected url\n' >&2
    exit 1
    ;;
esac
EOF

  cat >"${bin}/git" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
cmd=${1:-}
if [[ $# -gt 0 ]]; then
  shift
fi
case "$cmd" in
  clone)
    dest=
    for dest in "$@"; do
      :
    done
    if [[ -z "$dest" ]]; then
      printf 'git clone: no destination\n' >&2
      exit 1
    fi
    mkdir -p "$dest"
    printf '%s\n' 'pkgver=0.0.0' 'pkgrel=1' >"${dest}/PKGBUILD"
    ;;
  push)
    if [[ -z ${GIT_PUSH_RECORD:-} ]]; then
      printf 'git push: no record path\n' >&2
      exit 1
    fi
    printf '%s\n' "${GIT_SSH_COMMAND-}" >"$GIT_PUSH_RECORD"
    ;;
  *)
    ;;
esac
EOF

  cat >"${bin}/chown" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF

  cat >"${bin}/runuser" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
while [[ $# -gt 0 && $1 != -- ]]; do
  shift
done
if [[ ${1:-} != -- ]]; then
  printf 'runuser: missing --\n' >&2
  exit 1
fi
shift
exec "$@"
EOF

  cat >"${bin}/updpkgsums" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF

  cat >"${bin}/makepkg" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ ${1:-} == --printsrcinfo ]]; then
  printf '%s\n' '0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef'
  exit 0
fi
printf 'makepkg: unexpected arguments\n' >&2
exit 1
EOF

  cat >"${bin}/ssh-keyscan" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ -n ${SSH_KEYSCAN_FILE:-} ]]; then
  cat "$SSH_KEYSCAN_FILE"
  exit 0
fi
printf '%s\n' 'aur.archlinux.org ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIEuBKrPzbawxA/k2g6NcyV5jmqwJ2s+zpgZGZ7tpLIcN'
EOF

  cat >"${bin}/getent" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ ${1:-} == passwd && ${2:-} == 4242 ]]; then
  printf 'builder:x:4242:4242::%s:/bin/bash\n' "${PASSWD_HOME:?}"
  exit 0
fi
printf 'getent: unexpected arguments\n' >&2
exit 1
EOF

  cat >"${bin}/id" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ ${1:-} == -u && $# -eq 1 ]]; then
  printf '%s\n' 4242
  exit 0
fi
if [[ ${1:-} == -u && ${2:-} == builder ]]; then
  exit 0
fi
printf 'id: unexpected arguments\n' >&2
exit 1
EOF

  local mock
  for mock in curl git chown runuser updpkgsums makepkg ssh-keyscan getent id; do
    chmod 755 "${bin}/${mock}"
  done
}

launch() {
  local name=$1
  local keyscan_file=${2:-}
  local case_dir="${root}/${name}"
  local bash_home="${case_dir}/bash-home"
  local passwd_home="${case_dir}/passwd-home"
  local status=0

  mkdir -p "$bash_home" "$passwd_home"
  HOME="$bash_home" \
    TMPDIR="${root}/tmp" \
    PATH="${bin}:${original_path}" \
    AUR_SSH_PRIVATE_KEY="$test_key" \
    GITHUB_REPOSITORY='example/thinwire' \
    PASSWD_HOME="$passwd_home" \
    GIT_PUSH_RECORD="${case_dir}/git-push-record" \
    SSH_KEYSCAN_FILE="$keyscan_file" \
    bash "$script" >"${case_dir}/stdout" 2>"${case_dir}/stderr" || status=$?
  printf '%s\n' "$status" >"${case_dir}/status"
}

dump_case() {
  local case_dir=$1
  printf 'status=%s\n' "$(<"${case_dir}/status")" >&2
  printf '--- stdout ---\n' >&2
  cat "${case_dir}/stdout" >&2
  printf '--- stderr ---\n' >&2
  cat "${case_dir}/stderr" >&2
}

split_homes() {
  local name=split
  local case_dir="${root}/${name}"
  local bash_home="${case_dir}/bash-home"
  local passwd_home="${case_dir}/passwd-home"
  local out err record status known home_known key_path mode cmd claim

  launch "$name"
  out="${case_dir}/stdout"
  err="${case_dir}/stderr"
  record="${case_dir}/git-push-record"
  status=$(<"${case_dir}/status")
  known="${passwd_home}/.ssh/known_hosts"
  home_known="${bash_home}/.ssh/known_hosts"
  key_path="${passwd_home}/.ssh/aur"

  claim='split homes exit 0'
  if [[ "$status" -ne 0 ]]; then
    dump_case "$case_dir"
    fail "$claim"
  fi
  pass "$claim"

  claim='known_hosts exists only under the passwd home .ssh, not under HOME/.ssh'
  if [[ -f "$known" && ! -e "$home_known" ]]; then
    pass "$claim"
  else
    fail "$claim"
  fi

  claim='the key file is under the passwd home .ssh'
  if [[ -f "$key_path" && ! -e "${bash_home}/.ssh/aur" ]]; then
    pass "$claim"
  else
    fail "$claim"
  fi

  claim='the key file mode is 600'
  mode=$(stat -c '%a' "$key_path")
  if [[ "$mode" == 600 ]]; then
    pass "$claim"
  else
    fail "$claim"
  fi

  claim='the key file contents are the test key plus a newline'
  printf '%s\n' "$test_key" >"${case_dir}/expected-key"
  if cmp -s "${case_dir}/expected-key" "$key_path"; then
    pass "$claim"
  else
    fail "$claim"
  fi

  claim='the test key string does not appear in the script stdout or stderr'
  if grep -F -q -- "$test_key" "$out" "$err"; then
    fail "$claim"
  fi
  pass "$claim"

  claim='the recorded command contains StrictHostKeyChecking=yes, UserKnownHostsFile= and the passwd-home known_hosts path, and neither accept-new nor StrictHostKeyChecking=no'
  if [[ ! -f "$record" ]]; then
    fail "$claim"
  fi
  cmd=$(<"$record")
  if [[ "$cmd" == *'StrictHostKeyChecking=yes'* \
    && "$cmd" == *"UserKnownHostsFile=${known}"* \
    && "$cmd" != *'accept-new'* \
    && "$cmd" != *'StrictHostKeyChecking=no'* ]]; then
    pass "$claim"
  else
    fail "$claim"
  fi

  claim="stdout contains ${pushed_line}"
  if grep -F -q -x -e "$pushed_line" "$out"; then
    pass "$claim"
  else
    fail "$claim"
  fi
}

wrong_host_key() {
  local key_dir="${root}/wrong-key"
  local keyscan="${root}/wrong-keyscan"
  local name=wrong
  local case_dir="${root}/${name}"
  local out record status fp claim

  mkdir -p "$key_dir"
  ssh-keygen -t ed25519 -f "${key_dir}/id_ed25519" -N "" -q -C "aur-nightly-test"
  rm -f "${key_dir}/id_ed25519"
  awk '{ printf "aur.archlinux.org %s %s\n", $1, $2 }' "${key_dir}/id_ed25519.pub" >"$keyscan"
  fp=$(ssh-keygen -lf "$keyscan" | awk '{ print $2 }') || true
  if [[ -z "$fp" || "$fp" == "$pinned_fp" ]]; then
    fail 'generated host key is a different ed25519 public key'
  fi

  launch "$name" "$keyscan"
  out="${case_dir}/stdout"
  record="${case_dir}/git-push-record"
  status=$(<"${case_dir}/status")

  claim='wrong host key exit 1'
  if [[ "$status" -eq 1 ]]; then
    pass "$claim"
  else
    dump_case "$case_dir"
    fail "$claim"
  fi

  claim='wrong host key prints the fingerprint error'
  if grep -F -q -x -e "$fingerprint_error" "$out"; then
    pass "$claim"
  else
    dump_case "$case_dir"
    fail "$claim"
  fi

  claim='wrong host key records no push'
  if [[ -e "$record" ]]; then
    fail "$claim"
  fi
  pass "$claim"
}

install_mocks
split_homes
wrong_host_key
