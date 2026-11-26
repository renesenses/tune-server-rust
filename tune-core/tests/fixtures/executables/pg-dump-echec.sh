#!/bin/sh
while [ $# -gt 0 ]; do
  if [ "$1" = "--file" ]; then shift; printf 'moitie' > "$1"; fi
  shift
done
echo "pg_dump: error: password secret@mdp rejected" >&2
exit 1
