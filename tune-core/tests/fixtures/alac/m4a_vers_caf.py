#!/usr/bin/env python3
"""Remultiplexe un .m4a ALAC en .caf ALAC — SANS toucher aux paquets.

Sert uniquement a donner les paquets ALAC de la fixture, tels quels, au
decodeur de REFERENCE d'Apple (`alacconvert`, macosforge/alac), qui ne lit
que le conteneur CAF. Aucun reencodage : les octets ALAC du .m4a sont
recopies bit pour bit dans le .caf.
"""
import struct
import sys


def atomes(buf, debut, fin):
    p = debut
    while p + 8 <= fin:
        taille = struct.unpack_from(">I", buf, p)[0]
        kind = buf[p + 4:p + 8]
        entete = 8
        if taille == 1:
            taille = struct.unpack_from(">Q", buf, p + 8)[0]
            entete = 16
        elif taille == 0:
            taille = fin - p
        yield kind, p + entete, p + taille
        p += taille


def trouve(buf, chemin, debut=0, fin=None):
    """Descend une suite d'atomes, p.ex. ['moov','trak','mdia',...]."""
    fin = len(buf) if fin is None else fin
    for kind, d, f in atomes(buf, debut, fin):
        if kind == chemin[0]:
            if len(chemin) == 1:
                return d, f
            r = trouve(buf, chemin[1:], d, f)
            if r:
                return r
    return None


def main(src, dst):
    buf = open(src, "rb").read()

    stbl = trouve(buf, [b"moov", b"trak", b"mdia", b"minf", b"stbl"])
    assert stbl, "stbl introuvable"
    sd, sf = stbl

    # --- stsd -> entree `alac` -> boite `alac` (magic cookie)
    r = trouve(buf, [b"stsd"], sd, sf)
    assert r, "stsd introuvable"
    d, f = r
    d += 8  # version/flags + entry_count
    entree = next(atomes(buf, d, f))
    assert entree[0] == b"alac", f"entree stsd = {entree[0]!r}, ALAC attendu"
    ed, ef = entree[1], entree[2]
    canaux = struct.unpack_from(">H", buf, ed + 16)[0]
    cookie = None
    for kind, cd, cf in atomes(buf, ed + 28, ef):
        if kind == b"alac":
            cookie = buf[cd + 4:cf]  # saute version/flags
    assert cookie, "magic cookie ALAC introuvable"
    # ALACSpecificConfig : frameLength(4) .. bitDepth a l'octet 5
    frames_par_paquet = struct.unpack_from(">I", cookie, 0)[0]
    profondeur = cookie[5]
    drapeaux = {16: 1, 20: 2, 24: 3, 32: 4}[profondeur]

    # --- stsz : taille de chaque paquet
    r = trouve(buf, [b"stsz"], sd, sf)
    d, f = r
    taille_uniforme, nb = struct.unpack_from(">II", buf, d + 4)
    if taille_uniforme:
        tailles = [taille_uniforme] * nb
    else:
        tailles = list(struct.unpack_from(f">{nb}I", buf, d + 12))

    # --- stco / co64 : offset du premier bloc
    r = trouve(buf, [b"stco"], sd, sf)
    if r:
        d, f = r
        nb_blocs = struct.unpack_from(">I", buf, d + 4)[0]
        offsets = list(struct.unpack_from(f">{nb_blocs}I", buf, d + 8))
    else:
        d, f = trouve(buf, [b"co64"], sd, sf)
        nb_blocs = struct.unpack_from(">I", buf, d + 4)[0]
        offsets = list(struct.unpack_from(f">{nb_blocs}Q", buf, d + 8))

    # --- stsc : combien de paquets par bloc
    d, f = trouve(buf, [b"stsc"], sd, sf)
    nb_e = struct.unpack_from(">I", buf, d + 4)[0]
    entrees = [struct.unpack_from(">III", buf, d + 8 + 12 * i) for i in range(nb_e)]

    # Paquets, dans l'ordre du fichier, decoupes bloc par bloc.
    paquets = []
    i_paquet = 0
    for i_bloc in range(nb_blocs):
        par_bloc = entrees[0][1]
        for premier, n, _ in entrees:
            if i_bloc + 1 >= premier:
                par_bloc = n
        pos = offsets[i_bloc]
        for _ in range(par_bloc):
            if i_paquet >= len(tailles):
                break
            t = tailles[i_paquet]
            paquets.append(buf[pos:pos + t])
            pos += t
            i_paquet += 1
    assert i_paquet == len(tailles), f"{i_paquet}/{len(tailles)} paquets retrouves"

    # --- cadence et nombre de trames valides : mdhd (timescale, duration)
    d, f = trouve(buf, [b"moov", b"trak", b"mdia", b"mdhd"])
    version = buf[d]
    if version == 1:
        cadence = struct.unpack_from(">I", buf, d + 4 + 8 + 8)[0]
        trames = struct.unpack_from(">Q", buf, d + 4 + 8 + 8 + 4)[0]
    else:
        cadence = struct.unpack_from(">I", buf, d + 4 + 4 + 4)[0]
        trames = struct.unpack_from(">I", buf, d + 4 + 4 + 4 + 4)[0]

    # --- ecriture CAF
    out = bytearray()
    out += b"caff" + struct.pack(">HH", 1, 0)
    desc = struct.pack(">dIIIIII", float(cadence), 0x616C6163, drapeaux,
                       0, frames_par_paquet, canaux, 0)
    out += b"desc" + struct.pack(">q", len(desc)) + desc
    out += b"kuki" + struct.pack(">q", len(cookie)) + cookie

    def varint(v):
        b = bytearray([v & 0x7F])
        v >>= 7
        while v:
            b.insert(0, (v & 0x7F) | 0x80)
            v >>= 7
        return bytes(b)

    table = b"".join(varint(t) for t in tailles)
    pakt = struct.pack(">qqii", len(tailles), trames, 0, 0) + table
    out += b"pakt" + struct.pack(">q", len(pakt)) + pakt

    donnees = b"".join(paquets)
    out += b"data" + struct.pack(">q", 4 + len(donnees)) + struct.pack(">I", 0) + donnees

    open(dst, "wb").write(bytes(out))
    print(f"{src} -> {dst}: {len(tailles)} paquets, {trames} trames, "
          f"{canaux} ch, {cadence} Hz, {profondeur} bits, "
          f"{len(donnees)} octets ALAC recopies tels quels")


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
