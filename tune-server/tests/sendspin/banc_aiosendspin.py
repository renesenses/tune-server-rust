"""Banc d'interopérabilité Sendspin (#3326, S2-c) : le lecteur de RÉFÉRENCE.

Ce script fait d'``aiosendspin`` (implémentation Python de référence,
https://github.com/Sendspin/aiosendspin) un lecteur ``player@v1`` qui, au lieu
de jouer, ENREGISTRE ce qu'il reçoit :

- chaque flux (``stream/start``) dans un fichier ``flux-<n>.<pcm|flac>``,
  octets tels que reçus (FLAC : ``codec_header`` puis les trames) ;
- chaque morceau audio dans le journal JSON : horodatage serveur, taille,
  ``send_ahead``, heure d'arrivée locale, heure de lecture prédite par le filtre
  de temps (Kalman) d'aiosendspin, avance restante ;
- l'état du filtre de temps toutes les 250 ms (décalage, erreur, dérive) ;
- les messages de contrôle (``stream/*``, ``group/update``, ``server/command``).

Il s'appaire par la méthode « Pairing PSK » : il publie son jeton
(``SP:0…``) sur la sortie standard, le test le passe à la route opérateur de
Tune, et l'échange chiffré se fait entre les deux implémentations.

Lancé par ``lecteur_aiosendspin_3326.rs`` (test optionnel : il ne s'exécute que
si ``TUNE_AIOSENDSPIN_PYTHON`` désigne un interpréteur où ``aiosendspin`` et
``soundfile`` sont installés). La fermeture de l'entrée standard termine la
session : le script se déconnecte, écrit ``journal.json`` et sort. Une ligne
``FORMAT <codec:taux:bits:canaux>`` change la préférence de format en pleine
lecture (``client/state``), comme le ferait une enceinte réelle.

Aucune lecture audio, aucun ffmpeg : le FLAC est décodé par libsndfile
(``soundfile``) pour mesurer la durée de chaque trame.
"""

from __future__ import annotations

import argparse
import asyncio
import base64
import hashlib
import io
import json
import logging
import sys
from pathlib import Path

import soundfile
from aiosendspin.client.client import SendspinClient
from aiosendspin.models.player import ClientHelloPlayerSupport, SupportedAudioFormat
from aiosendspin.models.types import AudioCodec, PlayerCommand, Roles
from aiosendspin.noise.keys import Identity, generate_psk, psk_id_for
from aiosendspin.noise.pairing_token import PSKPairingToken, encode_psk_token
from aiosendspin.noise.trust_store import FileClientPairingStore, PairingPsk


def formats(spec: str) -> list[SupportedAudioFormat]:
    """``pcm:44100:16:2,flac:48000:24:2`` → formats annoncés, dans l'ordre."""
    sortie = []
    for item in spec.split(","):
        codec, taux, bits, canaux = item.split(":")
        sortie.append(
            SupportedAudioFormat(
                codec=AudioCodec(codec),
                sample_rate=int(taux),
                bit_depth=int(bits),
                channels=int(canaux),
            )
        )
    return sortie


def taille_de_bloc(trame: bytes) -> int:
    """Nombre d'échantillons par canal d'une trame FLAC, lu dans son en-tête."""
    code = trame[2] >> 4
    if code == 1:
        return 192
    if 2 <= code <= 5:
        return 576 << (code - 2)
    if code >= 8:
        return 256 << (code - 8)
    uns = 8 - (trame[4] ^ 0xFF).bit_length()  # longueur du numéro « UTF-8 »
    pos = 4 + max(1, uns)
    if code == 6:
        return trame[pos] + 1
    return int.from_bytes(trame[pos : pos + 2], "big") + 1


def decoder_trame(entete: bytes, trame: bytes):
    """Une trame FLAC isolée (après son en-tête), décodée par libsndfile.

    Le STREAMINFO d'un flux annonce un total d'échantillons inconnu (0), que
    libsndfile n'accepte pas : la copie qu'on lui donne porte le total exact
    de la trame, lu dans l'en-tête de la trame elle-même.
    """
    n = taille_de_bloc(trame)
    si = bytearray(entete)
    base = 8  # fLaC + en-tête du bloc STREAMINFO
    si[base + 13] = (si[base + 13] & 0xF0) | ((n >> 32) & 0x0F)
    si[base + 14 : base + 18] = (n & 0xFFFFFFFF).to_bytes(4, "big")
    with soundfile.SoundFile(io.BytesIO(bytes(si) + trame)) as f:
        donnees = f.read(dtype="int32", always_2d=True)
    if len(donnees) != n:
        raise ValueError(f"trame FLAC : {len(donnees)} echantillons decodes, {n} annonces")
    return donnees


class Banc:
    def __init__(self, client: SendspinClient, dossier: Path) -> None:
        self.client = client
        self.dossier = dossier
        self.evenements: list[dict] = []
        self.flux: list[dict] = []
        self.horloge: list[dict] = []
        self.courant: dict | None = None
        self.fichier = None
        self.volume = 100
        self.muted = False

    def t(self) -> int:
        return self.client.now_us()

    def evenement(self, nature: str, **champs) -> None:
        champs.update(type=nature, t_local=self.t())
        self.evenements.append(champs)
        logging.info("evenement %s", json.dumps(champs, default=str))

    def fermer_flux(self) -> None:
        if self.fichier is not None:
            self.fichier.close()
            self.fichier = None

    def stream_start(self, message) -> None:
        joueur = message.payload.player
        if joueur is None:
            return
        fmt = {
            "codec": joueur.codec.value,
            "sample_rate": joueur.sample_rate,
            "channels": joueur.channels,
            "bit_depth": joueur.bit_depth,
        }
        entete = (
            base64.b64decode(joueur.codec_header, validate=True)
            if joueur.codec_header
            else None
        )
        self.evenement(
            "stream/start",
            format=fmt,
            codec_header=bool(entete),
            server_transmitted=message.payload.server_transmitted,
        )
        self.ouvrir_flux(fmt, entete, "stream/start")

    def ouvrir_flux(self, fmt: dict, entete: bytes | None, cause: str) -> None:
        self.fermer_flux()
        n = len(self.flux)
        chemin = self.dossier / f"flux-{n}.{fmt['codec']}"
        self.fichier = chemin.open("wb")
        if entete:
            self.fichier.write(entete)
        self.courant = {
            "index": n,
            "format": fmt,
            "fichier": str(chemin),
            "entete": base64.b64encode(entete).decode() if entete else None,
            "cause": cause,
            "morceaux": [],
        }
        self.flux.append(self.courant)

    def stream_end(self, roles) -> None:
        self.evenement("stream/end", roles=roles)

    def stream_clear(self, roles) -> None:
        self.evenement("stream/clear", roles=roles)
        # Nouvelle ligne de temps, même format : un fichier à part.
        if self.courant is not None:
            entete = self.courant["entete"]
            self.ouvrir_flux(
                self.courant["format"],
                base64.b64decode(entete) if entete else None,
                "stream/clear",
            )

    def group_update(self, payload) -> None:
        self.evenement(
            "group/update",
            playback_state=getattr(payload.playback_state, "value", payload.playback_state),
            group_id=payload.group_id,
        )

    def server_command(self, payload) -> None:
        cmd = payload.player
        if cmd is None:
            return
        nature = getattr(cmd.command, "value", cmd.command)
        self.evenement("server/command", command=nature, volume=cmd.volume, mute=cmd.mute)
        if nature == "volume" and cmd.volume is not None:
            self.volume = cmd.volume
        elif nature == "mute" and cmd.mute is not None:
            self.muted = cmd.mute
        # La spécification : l'état DOIT être renvoyé quand il change, y
        # compris par une commande du serveur.
        asyncio.get_running_loop().create_task(
            self.client.send_player_state(available=True, volume=self.volume, muted=self.muted)
        )

    def duree_trames(self, donnees: bytes) -> int:
        fmt = self.courant["format"]
        if fmt["codec"] == "pcm":
            return len(donnees) // (fmt["channels"] * fmt["bit_depth"] // 8)
        return len(decoder_trame(base64.b64decode(self.courant["entete"]), donnees))

    def audio(self, timestamp_us: int, donnees: bytes, format_, send_ahead: int) -> None:
        arrivee = self.t()
        if self.courant is None:
            self.evenement("audio_hors_flux", ts=timestamp_us)
            return
        # Un stream/start EN PLACE (changement de format d'un flux actif)
        # n'atteint pas les écouteurs `stream_start` d'aiosendspin : il se voit
        # au format avec lequel aiosendspin livre le morceau, qui est celui en
        # vigueur à sa réception (règle de la spécification).
        fmt = {
            "codec": format_.codec.value,
            "sample_rate": format_.pcm_format.sample_rate,
            "channels": format_.pcm_format.channels,
            "bit_depth": format_.pcm_format.bit_depth,
        }
        if fmt != self.courant["format"]:
            self.evenement("stream/start", format=fmt, en_place=True)
            self.ouvrir_flux(fmt, format_.codec_header, "stream/start en place")
        synchro = self.client.is_time_synchronized()
        lecture = self.client.compute_play_time(timestamp_us) if synchro else None
        debut = self.fichier.tell()
        self.fichier.write(donnees)
        self.courant["morceaux"].append(
            {
                "debut": debut,
                "ts": timestamp_us,
                "octets": len(donnees),
                "trames": self.duree_trames(donnees),
                "send_ahead": send_ahead,
                "arrivee": arrivee,
                "lecture_predite": lecture,
                "synchro": synchro,
            }
        )

    async def surveiller_horloge(self) -> None:
        while True:
            await asyncio.sleep(0.25)
            connexion = getattr(self.client, "_admitted_connection", None)
            filtre = getattr(connexion, "_time_filter", None)
            if filtre is None or filtre.count == 0:
                continue
            self.horloge.append(
                {
                    "t_local": self.t(),
                    "count": filtre.count,
                    "synchro": filtre.is_synchronized,
                    "offset_us": filtre.offset,
                    "erreur_us": filtre.error if filtre.is_synchronized else None,
                    "derive": getattr(filtre, "_drift", None),
                }
            )

    def journal(self) -> dict:
        self.fermer_flux()
        for f in self.flux:
            f["sha256"] = hashlib.sha256(Path(f["fichier"]).read_bytes()).hexdigest()
            if f["format"]["codec"] == "flac" and f["morceaux"]:
                # Décodage INDÉPENDANT (libsndfile) : entiers natifs, int32 LE.
                # Morceau par morceau (en-tête + une trame) : le STREAMINFO d'un
                # flux annonce un total inconnu (0), que libsndfile ne lit pas
                # d'un bloc.
                brut = Path(f["fichier"]).read_bytes()
                entete = base64.b64decode(f["entete"])
                decale = 32 - f["format"]["bit_depth"]
                chemin = f["fichier"] + ".decode-int32le"
                with open(chemin, "wb") as sortie:
                    for m in f["morceaux"]:
                        trame = brut[m["debut"] : m["debut"] + m["octets"]]
                        donnees = decoder_trame(entete, trame)
                        sortie.write((donnees >> decale).astype("<i4").tobytes())
                f["decode"] = chemin
        return {"evenements": self.evenements, "flux": self.flux, "horloge": self.horloge}


async def principal() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--url", required=True)
    parser.add_argument("--dossier", required=True)
    parser.add_argument("--formats", required=True)
    parser.add_argument("--capacite", type=int, default=2_000_000)
    parser.add_argument("--lead-ms", type=float, default=250.0)
    parser.add_argument("--buffer-ms", type=float, default=250.0)
    args = parser.parse_args()
    dossier = Path(args.dossier)
    dossier.mkdir(parents=True, exist_ok=True)
    logging.basicConfig(
        filename=dossier / "aiosendspin.log",
        level=logging.DEBUG,
        format="%(asctime)s %(levelname)s %(name)s %(message)s",
    )

    identite = Identity.generate()
    magasin = await FileClientPairingStore.open(dossier / "appairage.json")
    psk = generate_psk()
    await magasin.set_pairing_psk(PairingPsk(psk_id=psk_id_for(psk), psk=psk))
    jeton = encode_psk_token(PSKPairingToken(client_id=identite.peer_id, pairing_psk=psk))

    client = SendspinClient(
        identite,
        "aiosendspin (banc Tune)",
        [Roles.PLAYER],
        pairing_store=magasin,
        player_support=ClientHelloPlayerSupport(
            supported_formats=formats(args.formats),
            buffer_capacity=args.capacite,
        ),
        state_supported_commands=[PlayerCommand.VOLUME, PlayerCommand.MUTE],
        required_lead_time_ms=args.lead_ms,
        min_buffer_ms=args.buffer_ms,
        initial_volume=100,
    )
    banc = Banc(client, dossier)
    client.add_stream_start_listener(banc.stream_start)
    client.add_stream_end_listener(banc.stream_end)
    client.add_stream_clear_listener(banc.stream_clear)
    client.add_group_update_listener(banc.group_update)
    client.add_server_command_listener(banc.server_command)
    client.add_audio_chunk_listener(banc.audio)

    print(f"CLIENT_ID={identite.peer_id}", flush=True)
    print(f"TOKEN={jeton}", flush=True)
    await client.connect(args.url)
    print("CONNECTE", flush=True)
    horloge = asyncio.get_running_loop().create_task(banc.surveiller_horloge())

    # Commandes du test, une par ligne ; la fermeture de l'entrée standard
    # termine la session.
    #   FORMAT pcm:48000:24:2   préférence de format (client/state `format`)
    while True:
        ligne = await asyncio.get_running_loop().run_in_executor(None, sys.stdin.readline)
        if not ligne:
            break
        mots = ligne.split()
        if mots[:1] == ["FORMAT"]:
            (prefere,) = formats(mots[1])
            banc.evenement("commande_format", format=mots[1])
            await client.set_preferred_format(prefere)
            print("FORMAT_ENVOYE", flush=True)
    horloge.cancel()
    await client.disconnect()
    (dossier / "journal.json").write_text(json.dumps(banc.journal(), indent=1))
    print("JOURNAL_ECRIT", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(principal()))
