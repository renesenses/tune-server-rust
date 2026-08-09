#!/usr/bin/env python3
"""
fetch_lrc.py — Récupère les paroles synchronisées (.lrc) depuis LRCLIB
pour toute une bibliothèque musicale (FLAC, DSF/DSD, MP3, M4A, OGG, WAV...).

Usage :
    pip install mutagen requests --break-system-packages
    python fetch_lrc.py --library "D:\Musique" --user-agent "MonPipeline/1.0 (contact@exemple.com)"

Options utiles :
    --dry-run           N'écrit rien, affiche juste ce qui serait fait
    --force              Retélécharge même si un .lrc existe déjà
    --save-plain          Si aucune version synchronisée n'existe, sauve les paroles brutes en .txt
    --log fetch_lrc.log   Fichier journal (par défaut: fetch_lrc.log dans le dossier courant)
    --sleep 0.3            Pause entre requêtes API (secondes)
    --ext .flac .dsf .mp3   Extensions à traiter (par défaut: liste large ci-dessous)

Le script est ré-exécutable sans risque : il saute les morceaux qui ont déjà
un .lrc (sauf --force), donc tu peux l'interrompre (Ctrl+C) et le relancer
plus tard sur une bibliothèque de 50 000 fichiers.
"""

import argparse
import sys
import time
import logging
from pathlib import Path

try:
    import requests
except ImportError:
    sys.exit("Il manque 'requests'. Installe avec : pip install requests --break-system-packages")

try:
    from mutagen import File as MutagenFile
except ImportError:
    sys.exit("Il manque 'mutagen'. Installe avec : pip install mutagen --break-system-packages")

LRCLIB_GET = "https://lrclib.net/api/get"
LRCLIB_SEARCH = "https://lrclib.net/api/search"

DEFAULT_EXTENSIONS = {
    ".flac", ".dsf", ".dff", ".mp3", ".m4a", ".ogg",
    ".opus", ".wav", ".ape", ".wma", ".aiff",
}


def setup_logging(log_path: Path):
    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s [%(levelname)s] %(message)s",
        handlers=[
            logging.FileHandler(log_path, encoding="utf-8"),
            logging.StreamHandler(sys.stdout),
        ],
    )


def read_tags(path: Path):
    """Retourne (artist, title, album, duration_sec) ou None si illisible/incomplet."""
    try:
        audio = MutagenFile(path, easy=True)
    except Exception as e:
        logging.warning(f"Lecture impossible {path.name}: {e}")
        return None

    if audio is None:
        return None

    def first(tag_list):
        return tag_list[0].strip() if tag_list else None

    tags = audio.tags or {}
    artist = first(tags.get("artist"))
    title = first(tags.get("title"))
    album = first(tags.get("album"))
    duration = int(round(audio.info.length)) if getattr(audio, "info", None) else None

    if not artist or not title or duration is None:
        return None
    return artist, title, album or "", duration


def query_lrclib(session, artist, title, album, duration, user_agent, timeout=10):
    """Interroge LRCLIB, avec repli sur /api/search si /api/get échoue."""
    headers = {"User-Agent": user_agent} if user_agent else {}
    params = {
        "artist_name": artist,
        "track_name": title,
        "album_name": album,
        "duration": duration,
    }

    r = session.get(LRCLIB_GET, params=params, headers=headers, timeout=timeout)
    if r.status_code == 429:
        wait = int(r.headers.get("Retry-After", "5"))
        logging.info(f"Rate limit atteint, pause {wait}s")
        time.sleep(wait)
        r = session.get(LRCLIB_GET, params=params, headers=headers, timeout=timeout)

    if r.status_code == 200:
        return r.json()

    # Repli : recherche floue, on garde le résultat dont la durée est la plus proche
    r = session.get(
        LRCLIB_SEARCH,
        params={"track_name": title, "artist_name": artist},
        headers=headers,
        timeout=timeout,
    )
    if r.status_code != 200:
        return None
    results = r.json()
    if not results:
        return None
    best = min(results, key=lambda x: abs(x.get("duration", 0) - duration))
    if abs(best.get("duration", 0) - duration) > 3:  # tolérance 3s
        return None
    return best


def process_file(session, path: Path, args):
    lrc_path = path.with_suffix(".lrc")
    if lrc_path.exists() and not args.force:
        return "skip"

    tags = read_tags(path)
    if tags is None:
        logging.warning(f"Tags incomplets, ignoré: {path}")
        return "no-tags"

    artist, title, album, duration = tags
    result = query_lrclib(session, artist, title, album, duration, args.user_agent)

    if result is None:
        logging.info(f"Introuvable: {artist} - {title}")
        return "not-found"

    synced = result.get("syncedLyrics")
    plain = result.get("plainLyrics")
    instrumental = result.get("instrumental")

    if instrumental:
        logging.info(f"Instrumental (pas de paroles): {artist} - {title}")
        return "instrumental"

    if synced:
        if not args.dry_run:
            lrc_path.write_text(synced, encoding="utf-8")
        logging.info(f"OK (synchronisé): {artist} - {title}")
        return "synced"

    if plain and args.save_plain:
        if not args.dry_run:
            path.with_suffix(".txt").write_text(plain, encoding="utf-8")
        logging.info(f"OK (paroles brutes seulement): {artist} - {title}")
        return "plain"

    logging.info(f"Paroles non synchronisées disponibles mais ignorées (utilise --save-plain): {artist} - {title}")
    return "plain-skipped"


def main():
    parser = argparse.ArgumentParser(description="Télécharge des .lrc depuis LRCLIB pour toute une bibliothèque musicale.")
    parser.add_argument("--library", required=True, help="Dossier racine de la bibliothèque musicale")
    parser.add_argument("--user-agent", default="fetch_lrc.py/1.0", help="User-Agent recommandé par LRCLIB (nom app + contact)")
    parser.add_argument("--ext", nargs="*", default=None, help="Extensions à traiter, ex: .flac .dsf")
    parser.add_argument("--force", action="store_true", help="Retélécharger même si le .lrc existe déjà")
    parser.add_argument("--save-plain", action="store_true", help="Sauver les paroles non synchronisées en .txt si pas de version synced")
    parser.add_argument("--dry-run", action="store_true", help="N'écrit aucun fichier, affiche seulement")
    parser.add_argument("--sleep", type=float, default=0.3, help="Pause entre requêtes API (secondes)")
    parser.add_argument("--log", default="fetch_lrc.log", help="Chemin du fichier journal")
    args = parser.parse_args()

    library = Path(args.library)
    if not library.exists():
        sys.exit(f"Dossier introuvable: {library}")

    extensions = {e.lower() if e.startswith(".") else f".{e.lower()}" for e in args.ext} if args.ext else DEFAULT_EXTENSIONS

    setup_logging(Path(args.log))
    logging.info(f"Démarrage — bibliothèque: {library} — extensions: {sorted(extensions)}")

    session = requests.Session()
    stats = {}

    files = [p for p in library.rglob("*") if p.suffix.lower() in extensions]
    total = len(files)
    logging.info(f"{total} fichiers audio trouvés")

    for i, path in enumerate(files, 1):
        try:
            outcome = process_file(session, path, args)
        except KeyboardInterrupt:
            logging.info("Interrompu par l'utilisateur.")
            break
        except Exception as e:
            logging.error(f"Erreur sur {path}: {e}")
            outcome = "error"

        stats[outcome] = stats.get(outcome, 0) + 1

        if outcome not in ("skip",):
            time.sleep(args.sleep)

        if i % 100 == 0:
            logging.info(f"Progression: {i}/{total}")

    logging.info("Terminé. Résumé :")
    for k, v in sorted(stats.items(), key=lambda x: -x[1]):
        logging.info(f"  {k}: {v}")


if __name__ == "__main__":
    main()
