#!/bin/sh
[ "$*" = --help ] || exit 99
case "$(basename "$0")" in
  success) exit 0 ;;
  help) echo help >&2; exit 1 ;;
  failed) exit 1 ;;
  found) echo help ;;
  slow) echo $$ > "$(dirname "$0")/pid"; exec sleep 30 ;;
  *) exit 99 ;;
esac
