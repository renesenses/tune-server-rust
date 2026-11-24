#!/bin/sh
D=$(dirname "$0")
printf '%s\n' "$*" > "$D/args"
EV=""
while [ $# -gt 0 ]; do
  case "$1" in
    --onevent) EV="$2"; shift ;;
  esac
  shift
done
while :; do
  if [ -f "$D/ordres" ]; then
    mv "$D/ordres" "$D/encours"
    while read A B C E; do
      case "$A" in
        ev) PLAYER_EVENT="$B" TRACK_ID="$C" VOLUME="$E" NAME="Titre $C" DURATION_MS=1000 $EV ;;
        pcm) head -c "$B" /dev/zero | tr '\000' '\125' ;;
        dors) sleep "$B" ;;
      esac
    done < "$D/encours"
    rm -f "$D/encours"
  fi
  sleep 0.05
done
