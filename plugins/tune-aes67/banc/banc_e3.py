#!/usr/bin/env python3
"""Banc d'essai de la Sortie AES67, étape E3 (#7336) : saut, enchaînement
sans blanc avec changement de cadence, pause et reprise.

Récepteur AES67 de test ÉCRIT INDÉPENDAMMENT du code Rust (mêmes outils
que banc_aes67.py : RFC 3550, 2974, 4566, 7273). Aucun son, rien hors de
127.0.0.1, ports libres non privilégiés.

  1. écrit trois pistes : un compteur 48 kHz 24 bits de 3 s (gauche = numéro
     de la trame, droite = son opposé), un sinus 1 kHz à 44,1 kHz de 1,5 s,
     un sinus 2 kHz à 96 kHz de 1 s ;
  2. lance l'émetteur `banc_e3` (exemple Rust), qui joue le compteur, saute
     à 2 s, enchaîne la 44,1 kHz puis la 96 kHz, et fait une pause de 300 ms
     dans la 96 kHz ;
  3. vérifie sur ce qu'il reçoit :
     - séquence RTP continue d'un bout à l'autre (aucune perte, aucun recul) ;
     - horodatage jamais en recul ni en chevauchement (pas >= 48 trames),
       exactement +48 hors paquets marqués ; trois paquets marqués : départ,
       saut, reprise ;
     - au saut et à la reprise, l'horodatage avance comme le temps d'arrivée
       (horloge média `direct`) ;
     - le saut tombe sur la trame 96 000 exactement, puis 96 000..143 999
       sans rien perdre ;
     - aux deux enchaînements, aucun paquet marqué, aucun silence, et le
       PCM qui suit le compteur a l'empreinte des deux pistes
       rééchantillonnées mises bout à bout (calculée par l'émetteur, hors
       réseau) ;
     - 1 kHz dans la première piste, 2 kHz dans la seconde, à 48 kHz ;
  4. écrit un pcap, le PCM reçu et rapport-e3.json ; sort en erreur si une
     vérification échoue.

Usage :
  python3 banc_e3.py <binaire banc_e3> <dossier de sortie>
"""
import hashlib
import json
import math
import os
import struct
import subprocess
import sys
import time
import wave

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from banc_aes67 import Ecoute, ecrire_pcap, goertzel, l24, lire_rtp, lire_sap  # noqa: E402


def ecrire_compteur(chemin, cadence=48000, trames=144000):
    with wave.open(chemin, "wb") as w:
        w.setnchannels(2)
        w.setsampwidth(3)
        w.setframerate(cadence)
        d = bytearray()
        for n in range(trames):
            d += struct.pack("<i", n)[:3] + struct.pack("<i", -n)[:3]
        w.writeframes(bytes(d))


def ecrire_sinus(chemin, cadence, frequence, duree_s):
    trames = int(round(duree_s * cadence))
    with wave.open(chemin, "wb") as w:
        w.setnchannels(2)
        w.setsampwidth(2)
        w.setframerate(cadence)
        d = bytearray()
        for n in range(trames):
            v = int(round(0.5 * math.sin(2 * math.pi * frequence * n / cadence) * 32767))
            d += struct.pack("<hh", v, v)
        w.writeframes(bytes(d))


def plus_long_silence(x, seuil=8192):
    long = cour = 0
    for v in x:
        cour = cour + 1 if abs(v) < seuil else 0
        long = max(long, cour)
    return long


def main():
    if len(sys.argv) != 3:
        print(__doc__)
        sys.exit(2)
    binaire, sortie = sys.argv[1], sys.argv[2]
    os.makedirs(sortie, exist_ok=True)
    compteur = os.path.join(sortie, "compteur-48000.wav")
    p44 = os.path.join(sortie, "sinus-44100-1k.wav")
    p96 = os.path.join(sortie, "sinus-96000-2k.wav")
    ecrire_compteur(compteur)
    ecrire_sinus(p44, 44100, 1000.0, 1.5)
    ecrire_sinus(p96, 96000, 2000.0, 1.0)

    rtp, sap = Ecoute(), Ecoute()
    rtp.start()
    sap.start()
    proc = subprocess.run(
        [binaire, compteur, p44, p96, "127.0.0.1:%d" % rtp.port, "127.0.0.1:%d" % sap.port],
        capture_output=True, text=True, timeout=120)
    time.sleep(0.3)
    rtp.arret.set()
    sap.arret.set()
    rtp.join()
    sap.join()
    if proc.returncode != 0:
        print(proc.stderr, file=sys.stderr)
        sys.exit("émetteur en échec")
    emetteur = json.loads(proc.stdout.strip().splitlines()[-1])
    ecrire_pcap(os.path.join(sortie, "flux-rtp-e3.pcap"), rtp.recus, rtp.port)

    paquets = [(t, *lire_rtp(d)) for t, _, _, d in rtp.recus]
    pcm = b"".join(c for _, _, c in paquets)
    with open(os.path.join(sortie, "pcm-recu-e3.l24"), "wb") as f:
        f.write(pcm)

    # 1. Transport : séquence et horodatage.
    pertes = desordres = reculs = sauts_non_marques = 0
    pas_min = None
    marques = [0] if paquets and paquets[0][1]["m"] else []
    reprises = []
    for i, ((t0, e0, _), (t1, e1, _)) in enumerate(zip(paquets, paquets[1:]), start=1):
        d = (e1["seq"] - e0["seq"]) & 0xFFFF
        if d == 0 or d > 0x8000:
            desordres += 1
        elif d > 1:
            pertes += d - 1
        pas = (e1["ts"] - e0["ts"]) & 0xFFFFFFFF
        if pas >= 0x80000000:
            pas -= 1 << 32
        pas_min = pas if pas_min is None else min(pas_min, pas)
        if pas < 48:
            reculs += 1
        if not e1["m"] and pas != 48:
            sauts_non_marques += 1
        if e1["m"]:
            marques.append(i)
            arrivee_ms = (t1 - t0) / 1e6
            reprises.append({"paquet": i, "pas_horodatage_trames": pas,
                             "pas_horodatage_ms": pas / 48.0,
                             "ecart_d_arrivee_ms": round(arrivee_ms, 3),
                             "difference_ms": round(pas / 48.0 - arrivee_ms, 3)})

    # 2. Contenu : le compteur, le saut, la suite.
    gauche = l24(pcm)[0::2]
    i_saut = next((i for i in range(1, len(gauche)) if gauche[i] == 96000 and gauche[i - 1] != 95999), None)
    avant_saut_ok = i_saut is not None and all(gauche[k] == k for k in range(i_saut))
    i_fin_compteur = next((i for i in range(i_saut or 0, len(gauche)) if gauche[i] == 143999), None)
    apres_saut_ok = (i_fin_compteur is not None
                     and gauche[i_saut:i_fin_compteur + 1] == list(range(96000, 144000)))
    debut_suite = (i_fin_compteur or 0) + 1
    suite = pcm[debut_suite * 6:]
    sha_suite = hashlib.sha256(suite).hexdigest()
    n44, n96 = emetteur["trames_par_piste_48k"]
    frontieres = {"compteur->44,1": debut_suite, "44,1->96": debut_suite + n44}
    silences = {k: plus_long_silence(gauche[max(0, f - 480):f + 480]) for k, f in frontieres.items()}
    marques_aux_frontieres = [k for k, f in frontieres.items()
                              if any(f // 48 - 1 <= m <= f // 48 + 1 for m in marques)]
    seg44 = [v / 8388608.0 for v in gauche[debut_suite + 4800:debut_suite + 4800 + 9600]]
    seg96 = [v / 8388608.0 for v in gauche[debut_suite + n44 + 4800:debut_suite + n44 + 4800 + 9600]]
    def gz(x, f):
        return goertzel(x, 48000, f) if x else 0.0

    g = {
        "piste44_1000": gz(seg44, 1000.0),
        "piste44_1088": gz(seg44, 1000.0 * 48000 / 44100),
        "piste96_2000": gz(seg96, 2000.0),
        "piste96_4000": gz(seg96, 4000.0),
    }
    annonces = [lire_sap(d) for _, _, _, d in sap.recus]
    versions = sorted({a["sdp"].split("\r\n")[1] for a in annonces if not a["retrait"]})

    rapport = {
        "paquets_rtp": len(paquets),
        "ssrc": sorted({e["ssrc"] for _, e, _ in paquets}),
        "pertes": pertes, "desordres": desordres,
        "reculs_ou_chevauchements": reculs, "pas_horodatage_min": pas_min,
        "sauts_non_marques": sauts_non_marques,
        "paquets_marques": marques, "nouvelles_origines": reprises,
        "saut": {"echantillon": i_saut, "trame_apres_saut": gauche[i_saut] if i_saut else None,
                 "compteur_continu_avant": avant_saut_ok, "compteur_continu_apres": apres_saut_ok},
        "frontieres_en_trames": frontieres,
        "trames_par_piste_48k": [n44, n96],
        "plus_long_silence_aux_frontieres": silences,
        "marques_aux_frontieres": marques_aux_frontieres,
        "suite_sha256_recu": sha_suite, "suite_sha256_attendu": emetteur["suite_sha256_attendu"],
        "suite_octets": [len(suite), emetteur["suite_octets_attendus"]],
        "goertzel": g,
        "sap": {"annonces": len(annonces), "lignes_o": versions},
        "emetteur": emetteur,
    }
    v = {
        "sequence_continue": pertes == 0 and desordres == 0 and len(rapport["ssrc"]) == 1,
        "aucun_recul_ni_chevauchement": reculs == 0,
        "plus_48_hors_marques": sauts_non_marques == 0,
        "trois_origines_depart_saut_reprise": len(marques) == 3,
        "horodatage_suit_l_arrivee": all(abs(r["difference_ms"]) < 5 for r in reprises),
        "saut_a_la_trame_pres": avant_saut_ok and apres_saut_ok and i_saut % 48 == 0,
        "enchainements_sans_marque": marques_aux_frontieres == [],
        "enchainements_sans_silence": all(s <= 2 for s in silences.values()),
        "suite_bit_a_bit": sha_suite == emetteur["suite_sha256_attendu"],
        "frequences": g["piste44_1000"] > 100 * g["piste44_1088"] > 0 and g["piste96_2000"] > 100 * g["piste96_4000"],
        "fin_naturelle": emetteur["evenements"][-1].get("fin_naturelle") is True,
        "sdp_inchange": len(versions) == 1,
    }
    rapport["verifications"] = v
    with open(os.path.join(sortie, "rapport-e3.json"), "w") as f:
        json.dump(rapport, f, indent=2, ensure_ascii=False)
    resume = {k: rapport[k] for k in ("paquets_rtp", "pertes", "reculs_ou_chevauchements",
                                      "pas_horodatage_min", "paquets_marques", "nouvelles_origines",
                                      "saut", "frontieres_en_trames", "plus_long_silence_aux_frontieres",
                                      "trames_par_piste_48k", "verifications")}
    resume["evenements"] = emetteur["evenements"]
    resume["echantillons_gauche_recus"] = len(gauche)
    resume["emission"] = emetteur["emission"]
    print(json.dumps(resume, indent=1, ensure_ascii=False))
    sys.exit(0 if all(v.values()) else 1)


if __name__ == "__main__":
    main()
