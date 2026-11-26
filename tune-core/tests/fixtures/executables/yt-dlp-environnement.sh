#!/bin/sh
VU="${TUNE_YOUTUBE_CLIENT_ID-absent}/${TUNE_YOUTUBE_CLIENT_SECRET-absent}"
if [ "$1" = "--version" ]; then echo "$VU"; exit 0; fi
echo "http://127.0.0.1:9/flux/$VU"
echo '{"User-Agent": "Mozilla/5.0"}'
echo 'tune-format=251,opus,128.93'
echo 'tune-disponible=NA'
