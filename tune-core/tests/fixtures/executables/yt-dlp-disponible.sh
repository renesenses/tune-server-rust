#!/bin/sh
TUNE_FAUX_FLUX=$(cat "$(dirname "$0")/flux-url")
DISPO=NA
for a in "$@"; do
  [ "$a" = 'tune-disponible=%(available_at)s' ] && DISPO=$(( $(date +%s) + 2 ))
done
echo "$TUNE_FAUX_FLUX/$DISPO"
echo '{"User-Agent": "Mozilla/5.0"}'
echo 'tune-format=251,opus,128.93'
echo "tune-disponible=$DISPO"
