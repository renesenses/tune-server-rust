#!/usr/bin/env bash
# Construire le greffon « Playlists converter » (#4717) et poser son
# `main.wasm` à côté de son manifeste.
#
# Le binaire wasm est COMMITÉ, comme celui du greffon Party : c'est ce qui
# permet à l'essai de bout en bout de charger un vrai module dans le bac à
# sable, au lieu de se croire sur parole. Ce script est la recette qui l'a
# produit — à rejouer après toute modification de la caisse.
#
#   bash plugins/tune-playlists-converter/construire.sh
#
# Prérequis : `rustup target add wasm32-unknown-unknown`.
set -euo pipefail

racine="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
destination="$racine/tune-server/tests/fixtures/plugins/playlists-converter"

cd "$racine"
cargo build \
    --package tune-playlists-converter \
    --target wasm32-unknown-unknown \
    --release

cible="${CARGO_TARGET_DIR:-$racine/target}"
produit="$cible/wasm32-unknown-unknown/release/tune_playlists_converter.wasm"

mkdir -p "$destination"
cp "$produit" "$destination/main.wasm"
ls -l "$destination/main.wasm"
