#!/usr/bin/env bash
# Decide quels jobs LOURDS de ci.yml une PR doit lancer.
#
# Entree : chemins Git separes par NUL sur STDIN
#          (`git diff --no-renames --name-only -z BASE HEAD`).
# Env    : CFG_PLATEFORME=true si le diff ajoute ou retire une ligne Rust/TOML
#          qui parle de plateforme (`cfg(windows)`, `target_os`, `[target.`...).
# Sortie : une ligne `cle=true|false` par famille de jobs, a ajouter telle
#          quelle a `$GITHUB_OUTPUT` :
#            windows    Windows + ASIO (compilation PR)
#            macos      macOS (compilation PR)
#            ffi        FFI mobile (compilation)
#            librespot  librespot (6 cibles)
#            livre      Test (jeu de fonctionnalites livre)
#            embedding  Audio embedding feature
#            audit      Security Audit
#
# Ce script ne sert QUE sur une PR dont la base n'est ni `main` ni `rc/*`.
# Push, lancement manuel, nuit, promotion et etiquette `ci:full` lancent tout
# sans le consulter (voir ci.yml, etape « Choisir les jobs lourds »).
#
# Fail-closed : un diff vide ou illisible allume tout. Un chemin du socle
# commun (manifestes, verrous, CI, empaquetage, build.rs) allume tout.
set -uo pipefail

FAMILLES="windows macos ffi librespot livre embedding audit"

tout_allumer() {
  local f
  for f in $FAMILLES; do printf '%s=true\n' "$f"; done
}

choisir_jobs_lourds() {
  local chemin bas nombre=0
  local windows=false macos=false ffi=false librespot=false livre=false embedding=false audit=false

  while IFS= read -r -d '' chemin; do
    [ -n "$chemin" ] || continue
    nombre=$((nombre + 1))
    bas="$(printf '%s' "$chemin" | tr '[:upper:]' '[:lower:]')"

    # Socle commun : tout changement ici peut casser n'importe quelle cible.
    case "$chemin" in
      Cargo.toml | */Cargo.toml | Cargo.lock | */Cargo.lock \
        | rust-toolchain | rust-toolchain.toml | .cargo/* | Cross.toml \
        | deny.toml | vendor/* | build.rs | */build.rs \
        | packaging/* | .github/workflows/* | .github/actions/* \
        | scripts/ci/* | scripts/detecter-jobs-lourds-ci.sh \
        | scripts/detecter-impact-ci.sh | scripts/determiner-profil-ci.sh)
        printf 'jobs lourds (socle commun) : %s\n' "$chemin" >&2
        tout_allumer
        return 0
        ;;
    esac

    case "$bas" in
      *windows* | *win32* | *wasapi* | *asio* | tune-desktop/*) windows=true ;;
    esac
    case "$bas" in
      *macos* | *darwin* | *coreaudio* | *apple* | tune-desktop/*) macos=true ;;
    esac
    case "$bas" in
      tune-ffi/* | *android* | *uniffi* | */ios/*) ffi=true ;;
    esac
    case "$bas" in
      *librespot* | *airplay-daemon* | *airplay_daemon*) librespot=true ;;
    esac
    case "$chemin" in
      plugins/* | sdk/* | tune-plugin-runtime-wasm/* | scripts/plugin-catalog.py \
        | tune-core/src/plugin_sdk.rs | tune-server/src/plugins_host.rs \
        | tune-server/src/premium_audio_plugins.rs) livre=true ;;
    esac
    case "$bas" in
      *embedding* | *onnx*) embedding=true ;;
    esac
  done

  if [ "$nombre" -eq 0 ]; then
    printf 'jobs lourds par securite : diff vide\n' >&2
    tout_allumer
    return 0
  fi

  if [ "${CFG_PLATEFORME:-false}" = true ]; then
    printf 'jobs lourds : ligne de plateforme (cfg/target) dans le diff\n' >&2
    windows=true
    macos=true
    ffi=true
  fi

  printf 'windows=%s\nmacos=%s\nffi=%s\nlibrespot=%s\nlivre=%s\nembedding=%s\naudit=%s\n' \
    "$windows" "$macos" "$ffi" "$librespot" "$livre" "$embedding" "$audit"
}

autotest() {
  local echecs=0

  attendu() {
    local libelle="$1" attendu_lignes="$2" cfg="$3"
    shift 3
    local obtenu
    if [ "$#" -eq 0 ]; then
      obtenu="$(CFG_PLATEFORME="$cfg" choisir_jobs_lourds </dev/null 2>/dev/null | tr '\n' ' ')"
    else
      obtenu="$(printf '%s\0' "$@" | CFG_PLATEFORME="$cfg" choisir_jobs_lourds 2>/dev/null | tr '\n' ' ')"
    fi
    if [ "$obtenu" = "$attendu_lignes" ]; then
      printf '  ok    %s\n' "$libelle"
    else
      printf '  ECHEC %s\n    attendu : %s\n    obtenu  : %s\n' "$libelle" "$attendu_lignes" "$obtenu"
      echecs=$((echecs + 1))
    fi
  }

  local TOUT='windows=true macos=true ffi=true librespot=true livre=true embedding=true audit=true '
  local RIEN='windows=false macos=false ffi=false librespot=false livre=false embedding=false audit=false '

  # La PR typique : du Rust du coeur, rien de plateforme.
  attendu 'tune-core/src seul' "$RIEN" false 'tune-core/src/db/album_repo.rs' 'tune-server/src/routes/playback.rs'
  # Fail-closed.
  attendu 'diff vide' "$TOUT" false
  attendu 'Cargo.lock' "$TOUT" false 'tune-core/src/a.rs' 'Cargo.lock'
  attendu 'Cargo.toml de caisse' "$TOUT" false 'tune-core/Cargo.toml'
  attendu 'verrou du moissonneur' "$TOUT" false 'tools/moissonneur-roon/Cargo.lock'
  attendu 'build.rs' "$TOUT" false 'tune-server/build.rs'
  attendu 'empaquetage' "$TOUT" false 'packaging/deb/control'
  attendu 'workflow' "$TOUT" false '.github/workflows/ci.yml'
  attendu 'ce script' "$TOUT" false 'scripts/detecter-jobs-lourds-ci.sh'
  attendu 'vendor' "$TOUT" false 'vendor/foo/src/lib.rs'
  # Familles.
  attendu 'sortie WASAPI' 'windows=true macos=false ffi=false librespot=false livre=false embedding=false audit=false ' false 'tune-core/src/audio/output/wasapi.rs'
  attendu 'CoreAudio' 'windows=false macos=true ffi=false librespot=false livre=false embedding=false audit=false ' false 'tune-core/src/audio/coreaudio_out.rs'
  attendu 'radios ne passe pas pour iOS' "$RIEN" false 'tune-core/src/radios_remi.rs'
  attendu 'tune-ffi' 'windows=false macos=false ffi=true librespot=false livre=false embedding=false audit=false ' false 'tune-ffi/src/lib.rs'
  attendu 'librespot' 'windows=false macos=false ffi=false librespot=true livre=false embedding=false audit=false ' false 'scripts/build-librespot.py'
  attendu 'airplay-daemon' 'windows=false macos=false ffi=false librespot=true livre=false embedding=false audit=false ' false 'scripts/build-airplay-daemon.py'
  attendu 'greffon' 'windows=false macos=false ffi=false librespot=false livre=true embedding=false audit=false ' false 'plugins/tune-aes67/src/banc.rs'
  attendu 'embedding' 'windows=false macos=false ffi=false librespot=false livre=false embedding=true audit=false ' false 'tune-core/src/audio/embedding/mod.rs'
  attendu 'tune-desktop' 'windows=true macos=true ffi=false librespot=false livre=false embedding=false audit=false ' false 'tune-desktop/src/main.rs'
  # Ligne de plateforme dans un fichier ordinaire (motif de #3074).
  attendu 'cfg(windows) dans le diff' 'windows=true macos=true ffi=true librespot=false livre=false embedding=false audit=false ' true 'tune-core/src/db/album_repo.rs'

  if [ "$echecs" -ne 0 ]; then
    printf '%s contre-epreuve(s) en echec\n' "$echecs" >&2
    return 1
  fi
  printf 'detecter-jobs-lourds-ci: contre-epreuves OK\n'
}

if [ "${1:-}" = "--autotest" ]; then
  autotest
else
  choisir_jobs_lourds
fi
