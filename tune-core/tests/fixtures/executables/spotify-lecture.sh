#!/bin/sh
D=$(dirname "$0")
printf '%s\n' "$*" > "$D/args"
printf '%s' "$LIBRESPOT_ACCESS_TOKEN" > "$D/jeton"
EV=""
while [ $# -gt 0 ]; do
  case "$1" in
    --onevent) EV="$2"; shift ;;
  esac
  shift
done
while :; do
  if [ -f "$D/jouer" ]; then
    read N P < "$D/jouer"
    rm -f "$D/jouer"
    case "$P" in
      *cleRefusee*)
        # #8565 : Spotify refuse la clé audio (librespot #1649).
        [ -n "$EV" ] && PLAYER_EVENT=loading TRACK_ID="$P" $EV
        echo "[2026-11-12T20:49:00Z ERROR librespot_core::audio_key] error audio key 0 1" >&2
        echo "[2026-11-12T20:49:00Z ERROR librespot_playback::player] Skipping to next track, unable to load track <SpotifyUri>: ()" >&2
        [ -n "$EV" ] && PLAYER_EVENT=unavailable TRACK_ID="$P" $EV
        continue ;;
    esac
    [ -n "$EV" ] && PLAYER_EVENT=track_changed TRACK_ID="$P" NAME="Titre essai" $EV
    [ -n "$EV" ] && PLAYER_EVENT=playing TRACK_ID="$P" POSITION_MS=0 $EV
    head -c "$N" /dev/zero | tr '\000' '\125'
    [ -n "$EV" ] && PLAYER_EVENT=end_of_track TRACK_ID="$P" $EV
  fi
  sleep 0.05
done
