#!/bin/sh
printf '%s\n' "$*" >> "$(dirname "$0")/args"
exec sleep 30
