#!/bin/sh
journal="$(dirname "$0")/journal.txt"
printf '%s\n' "$@" > "$journal"
echo "PGPASSWORD=$PGPASSWORD" >> "$journal"
while [ $# -gt 0 ]; do
  if [ "$1" = "--file" ]; then shift; printf 'PGDMP-faux' > "$1"; fi
  shift
done
