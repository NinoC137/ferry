#!/bin/sh
# Ferry plugin: sync an SSH target's runtime libraries and headers into a host sysroot.
# The Ferry plugin runner owns SSH option construction. This script never reads passwords.

set -eu

dest=""
use_sudo=1
delete=0

usage() {
  cat >&2 <<'EOF'
usage: sysroot-sync --dest <local-sysroot> [--no-sudo] [--delete]

Copies /lib, /usr/lib and /usr/include from the selected SSH device.  --delete
mirrors deletions from the device into the local sysroot.  It is intentionally off
by default because it removes host files beneath the selected destination.
EOF
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --dest)
      [ "$#" -ge 2 ] || { usage; exit 2; }
      dest=$2
      shift 2
      ;;
    --no-sudo)
      use_sudo=0
      shift
      ;;
    --delete)
      delete=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      printf 'sysroot-sync: unknown argument: %s\n' "$1" >&2
      usage
      exit 2
      ;;
  esac
done

[ -n "$dest" ] || { printf 'sysroot-sync: --dest is required\n' >&2; usage; exit 2; }
[ "$dest" = "~" ] && dest=$HOME
case "$dest" in
  "~/"*) dest="$HOME/${dest#\~/}" ;;
esac
[ -n "${FERRY_DEVICE_HOST:-}" ] || { printf 'sysroot-sync: Ferry did not provide an SSH host\n' >&2; exit 2; }
[ -n "${FERRY_DEVICE_USER:-}" ] || { printf 'sysroot-sync: Ferry did not provide an SSH user\n' >&2; exit 2; }
[ -n "${FERRY_SSH_RSH:-}" ] || { printf 'sysroot-sync: Ferry did not provide SSH options\n' >&2; exit 2; }

command -v rsync >/dev/null 2>&1 || { printf 'sysroot-sync: rsync is not installed on the host\n' >&2; exit 127; }

if [ "$use_sudo" -eq 1 ]; then
  command -v sudo >/dev/null 2>&1 || { printf 'sysroot-sync: sudo is not installed; use --no-sudo with a writable destination\n' >&2; exit 127; }
  if [ "${FERRY_PLUGIN_NONINTERACTIVE:-}" = "1" ]; then
    sudo_cmd='sudo -n'
  else
    sudo_cmd='sudo'
  fi
else
  sudo_cmd=''
fi

if [ "$use_sudo" -eq 1 ]; then
  # shellcheck disable=SC2086
  $sudo_cmd mkdir -p "$dest" "$dest/usr"
else
  mkdir -p "$dest" "$dest/usr"
fi

delete_arg=''
if [ "$delete" -eq 1 ]; then
  delete_arg='--delete'
fi

remote_prefix="${FERRY_DEVICE_USER}@${FERRY_DEVICE_HOST}:"
sync_dir() {
  source_path=$1
  target_parent=$2
  printf '\n==> %s%s -> %s\n' "$remote_prefix" "$source_path" "$target_parent"
  if [ "$use_sudo" -eq 1 ]; then
    # shellcheck disable=SC2086
    $sudo_cmd rsync -av $delete_arg -e "$FERRY_SSH_RSH" "${remote_prefix}${source_path}" "$target_parent"
  else
    # shellcheck disable=SC2086
    rsync -av $delete_arg -e "$FERRY_SSH_RSH" "${remote_prefix}${source_path}" "$target_parent"
  fi
}

sync_dir /lib "$dest/"
sync_dir /usr/lib "$dest/usr/"
sync_dir /usr/include "$dest/usr/"
printf '\nSysroot synchronized: %s\n' "$dest"
