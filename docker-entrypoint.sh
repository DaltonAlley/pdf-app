#!/bin/sh
set -eu

puid=${PUID:-1000}
pgid=${PGID:-1000}
case "$puid:$pgid" in
  *[!0-9:]*) echo "PUID and PGID must be positive integers" >&2; exit 1 ;;
esac
if ! [ "$puid" -ge 1 ] 2>/dev/null || ! [ "$pgid" -ge 1 ] 2>/dev/null; then
  echo "PUID and PGID must be positive integers" >&2
  exit 1
fi

data_dir=$(realpath -m -- "${PDF_TOOLS_DATA_DIR:-/app/data}")
case "$data_dir" in
  /app/data|/app/data/*) ;;
  *) echo "PDF_TOOLS_DATA_DIR must be /app/data or one of its subdirectories" >&2; exit 1 ;;
esac
export PDF_TOOLS_DATA_DIR=$data_dir

groupmod --non-unique --gid "$pgid" pdf-tools
usermod --non-unique --uid "$puid" --gid "$pgid" pdf-tools
mkdir -p -- "$data_dir"
chown -R -- pdf-tools:pdf-tools "$data_dir"

exec gosu pdf-tools "$@"
