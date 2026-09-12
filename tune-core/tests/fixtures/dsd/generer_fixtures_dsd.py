#!/usr/bin/env python3
"""Fabrique les fixtures DSD du banc d'empreintes (#2218, tranche T3).

Contenu 100 % synthetique : un modulateur sigma-delta du 2e ordre, ecrit ici,
module une sinusoide par canal. Aucune oeuvre, aucun droit a demander.

Les conteneurs sont ecrits d'apres les specifications publiees :
  - DSF  : « DSF File Format Specification » v1.01, Sony, 2005.
  - DSDIFF : « DSD Interchange File Format » v1.5, Philips, 2004.

Usage : python3 generer_fixtures_dsd.py <dossier-de-sortie>
"""

import math
import os
import struct
import sys

DSD64 = 2_822_400


def modulateur(freq_hz, amplitude, n_bits, graine):
    """Sigma-delta du 2e ordre — deterministe, sans dependance externe.

    Rend une liste de n_bits valeurs 0/1. `graine` decale la phase initiale de
    l'integrateur : deux canaux de meme frequence resteraient sinon identiques
    octet pour octet, et un echange de canaux ne se verrait pas.
    """
    bits = []
    i1 = (graine % 97) / 970.0
    i2 = (graine % 53) / 1060.0
    y = 1.0
    w = 2.0 * math.pi * freq_hz / DSD64
    for n in range(n_bits):
        x = amplitude * math.sin(w * n + graine * 0.017)
        i1 += x - y
        i2 += i1 - y
        # saturation des integrateurs : un sigma-delta du 2e ordre diverge sinon
        i1 = max(-2.0, min(2.0, i1))
        i2 = max(-2.0, min(2.0, i2))
        y = 1.0 if i2 >= 0.0 else -1.0
        bits.append(1 if y > 0.0 else 0)
    return bits


def empaqueter(bits, lsb_first):
    """8 bits par octet. DSF range le bit le plus ancien en poids FAIBLE,
    DSDIFF en poids FORT."""
    out = bytearray()
    for i in range(0, len(bits), 8):
        o = 0
        for k in range(8):
            b = bits[i + k]
            o |= b << (k if lsb_first else 7 - k)
        out.append(o)
    return bytes(out)


def canaux_dsd(freqs, octets_par_canal, lsb_first):
    """Un train d'octets par canal."""
    return [
        empaqueter(modulateur(f, 0.4, octets_par_canal * 8, 17 + 41 * i), lsb_first)
        for i, f in enumerate(freqs)
    ]


def ecrire_dsf(chemin, freqs, octets_par_canal, taille_bloc=4096):
    """DSF : blocs entrelaces PAR CANAL, dernier bloc complete de zeros."""
    canaux = canaux_dsd(freqs, octets_par_canal, lsb_first=True)
    nch = len(freqs)
    blocs = (octets_par_canal + taille_bloc - 1) // taille_bloc
    data = bytearray()
    for b in range(blocs):
        for c in canaux:
            tranche = c[b * taille_bloc : (b + 1) * taille_bloc]
            data += tranche + bytes(taille_bloc - len(tranche))
    total_samples = octets_par_canal * 8
    metadata_offset = 0  # pas d'ID3v2
    taille_fichier = 28 + 52 + 12 + len(data)

    buf = bytearray()
    buf += b"DSD "
    buf += struct.pack("<Q", 28)
    buf += struct.pack("<Q", taille_fichier)
    buf += struct.pack("<Q", metadata_offset)
    buf += b"fmt "
    buf += struct.pack("<Q", 52)
    buf += struct.pack("<I", 1)  # format version
    buf += struct.pack("<I", 0)  # format id : 0 = DSD brut
    buf += struct.pack("<I", 2 if nch == 2 else (7 if nch == 6 else nch))
    buf += struct.pack("<I", nch)
    buf += struct.pack("<I", DSD64)
    buf += struct.pack("<I", 1)  # bits per sample : 1 = LSB d'abord
    buf += struct.pack("<Q", total_samples)
    buf += struct.pack("<I", taille_bloc)
    buf += struct.pack("<I", 0)  # reserve
    buf += b"data"
    buf += struct.pack("<Q", 12 + len(data))
    buf += data
    open(chemin, "wb").write(bytes(buf))
    return bytes(data)


IDS_CANAUX = {
    2: [b"SLFT", b"SRGT"],
    6: [b"MLFT", b"MRGT", b"C   ", b"LFE ", b"LS  ", b"RS  "],
}


def _chunk(ident, charge):
    """Un chunk DSDIFF : ID, taille u64 BE, charge, remplissage a l'octet pair."""
    out = ident + struct.pack(">Q", len(charge)) + charge
    if len(charge) % 2:
        out += b"\x00"
    return out


def ecrire_dff(chemin, freqs, octets_par_canal, pstring_complete=True):
    """DSDIFF non compresse : octets entrelaces ch0 ch1 ch0 ch1 ..."""
    canaux = canaux_dsd(freqs, octets_par_canal, lsb_first=False)
    nch = len(freqs)
    data = bytearray()
    for i in range(octets_par_canal):
        for c in canaux:
            data.append(c[i])

    fver = _chunk(b"FVER", struct.pack(">I", 0x01050000))
    fs = _chunk(b"FS  ", struct.pack(">I", DSD64))
    chnl = _chunk(b"CHNL", struct.pack(">H", nch) + b"".join(IDS_CANAUX[nch]))
    # compressionName est un pstring DSDIFF : compte sur un octet, texte, PUIS
    # remplissage a l octet pair — et ce remplissage est COMPTE dans la taille
    # du chunk CMPR. 1 + 14 = 15, impair : un octet nul, donc CMPR = 20.
    # C est exactement ce qu ecrit wvunpack 5.8.1 (--dsdiff).
    nom = b"not compressed"
    pstring = bytes([len(nom)]) + nom
    if pstring_complete and len(pstring) % 2:
        pstring += b"\x00"
    cmpr = _chunk(b"CMPR", b"DSD " + pstring)
    prop = _chunk(b"PROP", b"SND " + fs + chnl + cmpr)
    dsd = _chunk(b"DSD ", bytes(data))
    corps = b"DSD " + fver + prop + dsd
    buf = b"FRM8" + struct.pack(">Q", len(corps)) + corps
    open(chemin, "wb").write(buf)
    return bytes(data)


def main():
    dest = sys.argv[1]
    os.makedirs(dest, exist_ok=True)
    # Stereo : 9 000 octets par canal, DELIBEREMENT hors multiple de 4 096 —
    # le dernier super-bloc DSF est donc complete de zeros, et l'extraction
    # doit les retrancher.
    ecrire_dsf(os.path.join(dest, "ref_dsd64_stereo.dsf"), [997.0, 1493.0], 9000)
    ecrire_dff(os.path.join(dest, "ref_dsd64_stereo.dff"), [997.0, 1493.0], 9000)
    # Multicanal 5.1 : 3 000 octets par canal.
    ecrire_dff(
        os.path.join(dest, "ref_dsd64_5v1.dff"),
        [997.0, 1493.0, 2003.0, 61.0, 3001.0, 4507.0],
        3000,
    )
    # Le MEME fichier que ref_dsd64_stereo.dff, en plus court, a une seule
    # difference pres : le pstring de CMPR n est PAS complete a l octet pair,
    # donc le chunk CMPR est de taille IMPAIRE. Le format l autorise (IFF :
    # tout chunk de taille impaire est suivi d un octet de remplissage), et
    # `parse_dff` s y desaligne. Voir la garde
    # `constat_un_chunk_cmpr_de_taille_impaire_fait_echouer_parse_dff`.
    ecrire_dff(
        os.path.join(dest, "ref_dsd64_stereo_cmpr_impair.dff"),
        [997.0, 1493.0],
        64,
        pstring_complete=False,
    )
    for n in sorted(os.listdir(dest)):
        p = os.path.join(dest, n)
        print(f"{n}  {os.path.getsize(p)} o.")


if __name__ == "__main__":
    main()
