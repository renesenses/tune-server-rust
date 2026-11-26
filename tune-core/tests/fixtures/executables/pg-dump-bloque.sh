#!/bin/sh
while [ $# -gt 0 ]; do
  if [ "$1" = "--file" ]; then shift; printf 'debut' > "$1"; fi
  shift
done
exec sleep 30
