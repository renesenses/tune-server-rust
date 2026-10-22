#!/usr/bin/env python3
"""Vecteurs de test du protocole Devialet AIR, propres a Tune (#6542).

Implementation de reference INDEPENDANTE, ecrite a partir de la seule
description des formats publiee par Remi (fil 2336 du forum). Aucun fichier,
aucun code ni aucun vecteur de son kit n'est repris ici. Avant d'ecrire ce
fichier, cette implementation a reproduit octet pour octet les 34 vecteurs de
son kit (paquets de Roon captures) ; la verification est decrite dans la PR.

Signaux synthetiques et deterministes, MAC fictive (adresse locale 02:00:5e:...).

Usage : python3 -I scripts/devialet-air-vecteurs.py > tune-core/src/outputs/devialet_air/vecteurs_tune.json
"""
import json
import math
import struct


def crc16(d):
    c = 0xFFFF
    for x in d:
        c ^= x << 8
        for _ in range(8):
            c = ((c << 1) ^ 0x1021) & 0xFFFF if c & 0x8000 else (c << 1) & 0xFFFF
    return c


IDX = {44100: 0, 48000: 1, 88200: 2, 96000: 3, 176400: 4, 192000: 5}


def tz(d, s):
    return -((-d) >> s) if d < 0 else d >> s


def bloc(e, cap):
    s = 0
    while True:
        rec = e[0]
        ch = []
        for x in e[1:]:
            q = tz(x - rec, s)
            rec += q << s
            ch.append((abs(q) << 1) | (1 if q < 0 else 0))
        b = max(ch).bit_length() if ch else 0
        if cap is None or b <= cap:
            break
        s += 1
    acc = 0
    for f in ch:
        acc = (acc << b) | f
    nb = (b * len(ch) + 7) // 8
    acc <<= nb * 8 - b * len(ch)
    return (e[0] & 0xFFFFFF).to_bytes(3, 'big') + bytes([s, s, b, b]) + (acc.to_bytes(nb, 'big') if nb else b'')


def dm(session, flux, fr, lecture, position, t):
    h = b'Dm\x02' + session + struct.pack('>IBQH', flux, (IDX[fr] << 1) | int(lecture), position, len(t))
    cap = 10 if fr > 96000 else None
    d = h + b''.join(bloc([x[c] for x in t], cap) for c in (0, 1))
    return d + struct.pack('>H', crc16(d)) + bytes(128)


def arret(session, flux):
    h = b'Dm\x00' + session + struct.pack('>I', flux) + bytes(11)
    return h + struct.pack('>H', crc16(h)) + bytes(128)


def dr(pkt, cmd, op, val):
    b = b'Dr' + struct.pack('>HHHf', pkt, cmd, op, val)
    return b + struct.pack('>H', crc16(b)) + bytes(128)


def df(session, flux, recue, paquet, jouee, naks):
    b = b'Df' + session + struct.pack('>IQQQ', flux, recue, paquet, jouee) + bytes(4) + bytes([len(naks)])
    for d, f in naks:
        b += struct.pack('>HH', d, f)
    return b + struct.pack('>H', crc16(b))


def da(mac, nom, sources, allume, tonalite, aucun_flux, source, sourdine, vol_db, aigus_db, graves_db, balance, preout, code_freq, horloge, session, flux):
    o = bytearray(345)
    o[0:2] = b'Da'
    o[2:4] = struct.pack('>H', 1234)
    o[4:7] = b'\x02\x10\x33'
    o[10] = flux
    o[12:18] = mac
    n = nom.encode()
    o[18] = len(n)
    o[19:19 + len(n)] = n
    for i in range(15):
        o[52 + 17 * i] = ord('0')  # '0' = source desactivee
    for i, (etat, sn) in sources.items():
        d = 52 + 17 * i
        o[d] = ord(etat)
        o[d + 1:d + 1 + len(sn)] = sn.encode()
    o[307] = (0x80 if allume else 0) | (0x10 if tonalite else 0)
    o[308] = (0x80 if aucun_flux else 0) | (source << 2) | (0x02 if sourdine else 0)
    o[310] = int(195 + 2 * vol_db)
    o[311] = int(aigus_db * 2) & 0xFF
    o[312] = int(graves_db * 2) & 0xFF
    o[313] = balance & 0xFF
    o[314] = preout
    o[318] = code_freq
    o[331:335] = struct.pack('>I', horloge)
    o[339:343] = session
    o[343:345] = struct.pack('>H', crc16(bytes(o[:343])))
    return bytes(o)


SESSION = bytes.fromhex('5a17c0de')
MAX24 = (1 << 23) - 1


def sinus(fr, hz, amp, phase=0.0):
    return [[int(round(amp * MAX24 * math.sin(2 * math.pi * hz * (i / fr) + phase))),
             int(round(amp * MAX24 * math.sin(2 * math.pi * hz * (i / fr) + phase + 1.0)))] for i in range(200)]


def lcg(graine):
    x = graine
    while True:
        x = (1103515245 * x + 12345) & 0x7FFFFFFF
        yield x


def bruit():
    g = lcg(6542)
    return [[(next(g) % (1 << 24)) - (1 << 23), (next(g) % (1 << 24)) - (1 << 23)] for _ in range(200)]


def main():
    cas = [
        ('silence', 44100, False, 0, [[0, 0]] * 200),
        ('constante +1000/-1000', 44100, False, 22000, [[1000, -1000]] * 200),
        ('rampe +n/-n', 44100, False, 22200, [[i, -i] for i in range(200)]),
        ('parabole', 44100, True, 88200, [[i * i * 7 - 50000, -(i * i * 5)] for i in range(200)]),
        ('impulsion isolee', 44100, True, 88400, [[(MAX24 if i == 100 else 0), 0] for i in range(200)]),
        ('bit isole', 44100, True, 88600, [[(1 << 12) if i % 37 == 0 else 0, 1 if i == 199 else 0] for i in range(200)]),
        ('bruit pleine echelle', 44100, True, 88800, bruit()),
        ('sinus 1 kHz 44,1 kHz -6 dBFS', 44100, True, 89000, sinus(44100, 1000, 0.5)),
        ('gauche seule', 44100, True, 89200, [[x[0], 0] for x in sinus(44100, 440, 0.3)]),
        ('sinus 16 bits cadre en 24', 44100, True, 89400, [[(x[0] >> 8) << 8, (x[1] >> 8) << 8] for x in sinus(44100, 997, 0.7)]),
        ('sinus 48 kHz', 48000, True, 96000, sinus(48000, 1000, 0.5)),
        ('sinus 88,2 kHz', 88200, True, 176400, sinus(88200, 5000, 0.5)),
        ('sinus 96 kHz', 96000, True, 192000, sinus(96000, 7000, 0.5)),
        ('extremes 24 bits', 96000, True, 192200, [[MAX24 if i % 2 else -(1 << 23), -(1 << 23) if i % 2 else MAX24] for i in range(200)]),
        ('sinus 176,4 kHz AVEC PERTE', 176400, True, 352800, sinus(176400, 3000, 0.5)),
        ('sinus 192 kHz AVEC PERTE', 192000, True, 384000, sinus(192000, 3000, 0.9)),
    ]
    dm_audio = []
    for i, (nom, fr, lecture, pos, t) in enumerate(cas):
        flux = 0x6542_0010 + i
        dm_audio.append({
            'nom': nom, 'session': SESSION.hex(), 'flux': flux, 'frequence': fr,
            'lecture': lecture, 'position': pos, 'trames': t,
            'avec_perte': fr > 96000,
            'paquet_hex': dm(SESSION, flux, fr, lecture, pos, t).hex(),
        })
    dm_arret = [{'session': SESSION.hex(), 'flux': f, 'paquet_hex': arret(SESSION, f).hex()} for f in (0x65420011, 0xFFFFFFFF)]
    cmds = [(0x0004, -30.0), (0x0004, -10.0), (0x0004, -80.0), (0x0004, -40.5), (0x0107, 0.0),
            (0x0007, 0.0), (0x0101, 0.0), (0x0001, 0.0), (0x0005, 9.0), (0x0005, 13.0)]
    dr_commande = []
    for k, (op, v) in enumerate(cmds):
        pkt = 100 + 2 * k
        dr_commande.append({'compteur_paquet': pkt, 'compteur_commande': pkt >> 1, 'opcode': op,
                            'valeur': v, 'paquet_hex': dr(pkt, pkt >> 1, op, v).hex()})
    mac = bytes([0x02, 0x00, 0x5e, 0x00, 0x00, 0x01])
    sources = {6: ('1', 'TV'), 9: ('4', 'AIR'), 13: ('1', 'Phono')}
    da_annonce = [{
        'paquet_hex': da(mac, 'EXPERT-TEST', sources, True, True, False, 9, True, -31.5, 3.0, -4.5,
                         -28, 0x1c, 11, 987654321, SESSION, 0x13).hex(),
        'attendu': {'mac': '02:00:5e:00:00:01', 'nom': 'EXPERT-TEST', 'allume': True, 'tonalite_active': True,
                    'aucun_flux_air': False, 'source_choisie': 9, 'sourdine': True, 'volume_db': -31.5,
                    'aigus_demi_db': 6, 'graves_demi_db': -9, 'balance_pour_cent': -28, 'pre_out': 28,
                    'frequence_entree': 96000, 'horloge_ms': 987654321, 'session': SESSION.hex(),
                    'flux_en_cours': 0x13, 'sources_actives': [6, 9, 13], 'source_9': 'AIR'},
    }, {
        'paquet_hex': da(mac, 'EXPERT-TEST', sources, False, False, True, 6, False, -97.5, 0, 0,
                         39, 0, 0, 5, SESSION, 0).hex(),
        'attendu': {'mac': '02:00:5e:00:00:01', 'nom': 'EXPERT-TEST', 'allume': False, 'tonalite_active': False,
                    'aucun_flux_air': True, 'source_choisie': 6, 'sourdine': False, 'volume_db': -97.5,
                    'aigus_demi_db': 0, 'graves_demi_db': 0, 'balance_pour_cent': 39, 'pre_out': 0,
                    'frequence_entree': None, 'horloge_ms': 5, 'session': SESSION.hex(),
                    'flux_en_cours': 0, 'sources_actives': [6, 9, 13], 'source_9': 'AIR'},
    }]
    df_retour = []
    for naks, jouee in (([], 0), ([(0, 0), (3, 5)], 44000), ([(1, 1), (2, 2), (10, 12)], 123456)):
        recl = []
        base = jouee // 200
        for d, f in naks:
            recl += [(base + k) * 200 for k in range(d, f + 1)]
        df_retour.append({'session': SESSION.hex(), 'flux': 0x65420010, 'derniere_trame_recue': jouee + 88199,
                          'dernier_paquet_recu': jouee + 88000, 'position_jouee': jouee, 'manquants': naks,
                          'positions_reclamees': recl,
                          'paquet_hex': df(SESSION, 0x65420010, jouee + 88199, jouee + 88000, jouee, naks).hex()})
    tout = {
        'origine': 'scripts/devialet-air-vecteurs.py (Tune, #6542) : signaux synthetiques, MAC fictive',
        'crc_catalogue': {'entree': '123456789', 'crc': 0x29B1},
        'dm_audio': dm_audio, 'dm_arret': dm_arret, 'dr_commande': dr_commande,
        'da_annonce': da_annonce, 'df_retour': df_retour,
    }
    # Un vecteur par ligne : lisible dans un diff, sans une ligne par trame.
    lignes = ['{']
    cles = list(tout)
    for i, k in enumerate(cles):
        fin = ',' if i < len(cles) - 1 else ''
        v = tout[k]
        if isinstance(v, list):
            lignes.append(f' {json.dumps(k)}: [')
            lignes += [f'  {json.dumps(x, ensure_ascii=False)}' + (',' if j < len(v) - 1 else '') for j, x in enumerate(v)]
            lignes.append(' ]' + fin)
        else:
            lignes.append(f' {json.dumps(k)}: {json.dumps(v, ensure_ascii=False)}{fin}')
    lignes.append('}')
    print('\n'.join(lignes))


if __name__ == '__main__':
    main()
