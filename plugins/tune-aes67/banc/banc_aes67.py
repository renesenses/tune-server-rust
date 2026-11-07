#!/usr/bin/env python3
"""Banc d'essai de la Sortie AES67 (#7336, étapes E0-E1).

Récepteur AES67 de test ÉCRIT INDÉPENDAMMENT du code Rust : il ne partage
avec l'émetteur que les normes (RFC 3550, 3190, 2974, 4566, 7273).

Ce qu'il fait, sans aucun son et sans quitter la boucle locale :
  1. écrit une piste de test WAV 44,1 kHz (gauche 1 kHz, droite 440 Hz) ;
  2. écoute en UDP sur 127.0.0.1 (ports libres, non privilégiés) le RTP et le
     SAP ;
  3. lance l'émetteur du banc (exemple Rust `banc_emetteur`) vers ces ports ;
  4. écrit ce qu'il a reçu dans un pcap (LINKTYPE_RAW, en-têtes IPv4/UDP
     reconstitués : ce n'est pas une capture d'interface, tcpdump demande des
     droits que le banc n'a pas) et le PCM brut L24 dans un fichier ;
  5. vérifie les en-têtes, la continuité, la cadence d'arrivée, le SDP
     annoncé, l'empreinte SHA-256 du PCM contre celle que l'émetteur annonce,
     et que la gauche est bien un 1 kHz à 48 kHz (rééchantillonnage réel) ;
  6. écrit un rapport JSON et sort en erreur si une vérification échoue.

Usage :
  python3 banc_aes67.py <binaire banc_emetteur> <dossier de sortie>

Python 3 standard seulement.
"""
import hashlib
import json
import math
import os
import socket
import struct
import subprocess
import sys
import threading
import time
import wave


def ecrire_wav(chemin, cadence=44100, duree_s=3.0, frequences=(1000.0, 440.0)):
    trames = int(round(duree_s * cadence))
    with wave.open(chemin, "wb") as w:
        w.setnchannels(len(frequences))
        w.setsampwidth(2)
        w.setframerate(cadence)
        donnees = bytearray()
        for n in range(trames):
            t = n / cadence
            for f in frequences:
                donnees += struct.pack("<h", int(round(0.5 * math.sin(2 * math.pi * f * t) * 32767)))
        w.writeframes(bytes(donnees))


class Ecoute(threading.Thread):
    def __init__(self):
        super().__init__(daemon=True)
        self.s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.s.bind(("127.0.0.1", 0))
        self.s.settimeout(0.02)
        self.port = self.s.getsockname()[1]
        self.recus = []
        self.arret = threading.Event()

    def run(self):
        while not self.arret.is_set():
            try:
                data, src = self.s.recvfrom(65536)
            except socket.timeout:
                continue
            self.recus.append((time.monotonic_ns(), time.time_ns(), src, data))


def somme_ip(entete):
    s = sum(struct.unpack("!10H", entete))
    while s > 0xFFFF:
        s = (s & 0xFFFF) + (s >> 16)
    return (~s) & 0xFFFF


def ecrire_pcap(chemin, recus, port_dest):
    with open(chemin, "wb") as f:
        f.write(struct.pack("<IHHiIII", 0xA1B2C3D4, 2, 4, 0, 0, 65535, 101))
        for _, unix_ns, (ip, port), data in recus:
            udp = struct.pack("!HHHH", port, port_dest, 8 + len(data), 0) + data
            ip_src = socket.inet_aton(ip)
            ip_dst = socket.inet_aton("127.0.0.1")
            e = struct.pack("!BBHHHBBH4s4s", 0x45, 0, 20 + len(udp), 0, 0x4000, 64, 17, 0, ip_src, ip_dst)
            e = e[:10] + struct.pack("!H", somme_ip(e)) + e[12:]
            trame = e + udp
            f.write(struct.pack("<IIII", unix_ns // 10**9, (unix_ns // 1000) % 10**6, len(trame), len(trame)))
            f.write(trame)


def lire_rtp(p):
    if len(p) < 12:
        raise ValueError("court")
    b0, b1, seq, ts, ssrc = struct.unpack("!BBHII", p[:12])
    if b0 >> 6 != 2 or b0 & 0x3F:
        raise ValueError("en-tête RTP inattendu %02x" % b0)
    return {"m": bool(b1 & 0x80), "pt": b1 & 0x7F, "seq": seq, "ts": ts, "ssrc": ssrc}, p[12:]


def lire_sap(p):
    b0 = p[0]
    if b0 >> 5 != 1 or b0 & 0x13:
        raise ValueError("en-tête SAP inattendu %02x" % b0)
    auth = p[1] * 4
    reste = p[8 + auth:]
    if not reste.startswith(b"v=0"):
        nul = reste.index(0)
        if reste[:nul] != b"application/sdp":
            raise ValueError("type de charge SAP")
        reste = reste[nul + 1:]
    return {"retrait": bool(b0 & 0x04), "hachage": struct.unpack("!H", p[2:4])[0],
            "source": socket.inet_ntoa(p[4:8]), "sdp": reste.decode()}


def l24(octets):
    v = []
    for i in range(0, len(octets) - 2, 3):
        x = (octets[i] << 16) | (octets[i + 1] << 8) | octets[i + 2]
        v.append(x - (1 << 24) if x & 0x800000 else x)
    return v


def goertzel(x, cadence, f):
    w = 2 * math.pi * f / cadence
    c = 2 * math.cos(w)
    s1 = s2 = 0.0
    for e in x:
        s1, s2 = e + c * s1 - s2, s1
    return (s1 * s1 + s2 * s2 - c * s1 * s2) / (len(x) ** 2)


def main():
    if len(sys.argv) != 3:
        print(__doc__)
        sys.exit(2)
    binaire, sortie = sys.argv[1], sys.argv[2]
    os.makedirs(sortie, exist_ok=True)
    wav = os.path.join(sortie, "piste-44100-1k-440.wav")
    ecrire_wav(wav)

    rtp, sap = Ecoute(), Ecoute()
    rtp.start()
    sap.start()
    proc = subprocess.run(
        [binaire, wav, "127.0.0.1:%d" % rtp.port, "127.0.0.1:%d" % sap.port],
        capture_output=True, text=True, timeout=60)
    time.sleep(0.3)
    rtp.arret.set()
    sap.arret.set()
    rtp.join()
    sap.join()
    if proc.returncode != 0:
        print(proc.stderr, file=sys.stderr)
        sys.exit("émetteur en échec")
    emetteur = json.loads(proc.stdout.strip().splitlines()[-1])

    ecrire_pcap(os.path.join(sortie, "flux-rtp.pcap"), rtp.recus, rtp.port)
    ecrire_pcap(os.path.join(sortie, "annonces-sap.pcap"), sap.recus, sap.port)

    paquets = [(t, *lire_rtp(d)) for t, _, _, d in rtp.recus]
    pcm = b"".join(c for _, _, c in paquets)
    with open(os.path.join(sortie, "pcm-recu.l24"), "wb") as f:
        f.write(pcm)

    pertes = sauts = desordres = 0
    intervalles = []
    for (t0, e0, _), (t1, e1, _) in zip(paquets, paquets[1:]):
        d = (e1["seq"] - e0["seq"]) & 0xFFFF
        if d == 0 or d > 0x8000:
            desordres += 1
        elif d > 1:
            pertes += d - 1
        if not e1["m"]:
            if (e1["ts"] - e0["ts"]) & 0xFFFFFFFF != 48:
                sauts += 1
            intervalles.append((t1 - t0) / 1000.0)
    intervalles.sort()
    centile = lambda q: intervalles[min(len(intervalles) - 1, int(q * len(intervalles)))] if intervalles else None

    echantillons = l24(pcm)
    gauche = [v / 8388608.0 for v in echantillons[0::2][4800:4800 + 48000]]
    droite = [v / 8388608.0 for v in echantillons[1::2][4800:4800 + 48000]]
    g1000, g1088 = goertzel(gauche, 48000, 1000.0), goertzel(gauche, 48000, 1000.0 * 48000 / 44100)
    d440, d1000 = goertzel(droite, 48000, 440.0), goertzel(droite, 48000, 1000.0)

    annonces = [lire_sap(d) for _, _, _, d in sap.recus]
    sdp = annonces[0]["sdp"] if annonces else ""
    lignes_sdp = sdp.split("\r\n")

    rapport = {
        "paquets_rtp": len(paquets),
        "types_de_charge": sorted({e["pt"] for _, e, _ in paquets}),
        "ssrc": sorted({e["ssrc"] for _, e, _ in paquets}),
        "tailles_de_charge": sorted({len(c) for _, _, c in paquets}),
        "marqueurs": sum(1 for _, e, _ in paquets if e["m"]),
        "pertes": pertes, "desordres": desordres, "sauts_d_horodatage": sauts,
        "intervalle_us": {"moyen": sum(intervalles) / len(intervalles) if intervalles else None,
                          "p50": centile(0.5), "p99": centile(0.99), "max": intervalles[-1] if intervalles else None},
        "octets_pcm": len(pcm),
        "sha256_recu": hashlib.sha256(pcm).hexdigest(),
        "sha256_attendu": emetteur["sha256_attendu"],
        "goertzel": {"gauche_1000": g1000, "gauche_1088": g1088, "droite_440": d440, "droite_1000": d1000},
        "sap": {"paquets": len(annonces), "annonces": sum(1 for a in annonces if not a["retrait"]),
                "dernier_est_un_retrait": bool(annonces) and annonces[-1]["retrait"],
                "source": annonces[0]["source"] if annonces else None},
        "sdp": lignes_sdp,
        "emetteur": emetteur,
    }
    verifications = {
        "en_tetes": rapport["types_de_charge"] == [98] and len(rapport["ssrc"]) == 1,
        "charge_288_octets": rapport["tailles_de_charge"] == [288],
        "continuite": pertes == 0 and desordres == 0 and sauts == 0 and rapport["marqueurs"] == 1,
        "empreinte": rapport["sha256_recu"] == rapport["sha256_attendu"],
        "cadence_1_ms": intervalles and abs(rapport["intervalle_us"]["moyen"] - 1000) < 50,
        "reechantillonnage_1_khz": g1000 > 100 * g1088 and d440 > 100 * d1000,
        "sdp": "a=rtpmap:98 L24/48000/2" in lignes_sdp and "a=ptime:1" in lignes_sdp
               and "a=ts-refclk:local" in lignes_sdp and "a=mediaclk:direct=0" in lignes_sdp,
        "sap_annonce_puis_retrait": rapport["sap"]["annonces"] >= 1 and rapport["sap"]["dernier_est_un_retrait"],
    }
    rapport["verifications"] = verifications
    with open(os.path.join(sortie, "rapport.json"), "w") as f:
        json.dump(rapport, f, indent=2, ensure_ascii=False)
    print(json.dumps({k: rapport[k] for k in ("paquets_rtp", "pertes", "intervalle_us", "sha256_recu",
                                              "sha256_attendu", "verifications")}, indent=2))
    sys.exit(0 if all(verifications.values()) else 1)


if __name__ == "__main__":
    main()
