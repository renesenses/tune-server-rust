#!/usr/bin/env python3
"""Regenerate tune-core/src/discovery/oui_audio.rs from the Wireshark manuf
database (IEEE OUI registry).

Usage:
    curl -sL -o /tmp/manuf https://www.wireshark.org/download/automated/data/manuf
    python3 scripts/gen_oui_audio.py /tmp/manuf > tune-core/src/discovery/oui_audio.rs

Only 24-bit prefixes are kept, filtered to audio/renderer brands, sorted for
binary search. Never hand-edit the generated file: brand attribution must come
from the registry, not from memory.
"""
import re
import sys

BRANDS = [
    (r'\bApple\b', 'Apple'), (r'\bGoogle\b', 'Google'), (r'\bSonos\b', 'Sonos'),
    (r'\bYamaha\b', 'Yamaha'), (r'\bDenon\b', 'Denon'), (r'\bMarantz\b', 'Marantz'),
    (r'D&M Holdings|D&M ', 'Denon / Marantz'), (r'\bLenbrook\b|\bBluesound\b|NAD Electronics', 'Bluesound / NAD'),
    (r'\bLinn Products\b', 'Linn'), (r'\bNaim Audio\b', 'Naim'), (r'Cambridge Audio|Audio Partnership', 'Cambridge Audio'),
    (r'\bDevialet\b', 'Devialet'), (r'Bang & Olufsen', 'Bang & Olufsen'), (r'\bKEF\b|GP Acoustics', 'KEF'),
    (r'\bArcam\b', 'Arcam'), (r'\bRotel\b', 'Rotel'), (r'\bOnkyo\b', 'Onkyo'), (r'\bPioneer\b', 'Pioneer'),
    (r'\bTEAC\b', 'TEAC'), (r'\bAurender\b', 'Aurender'), (r'AURALiC|Auralic', 'AURALiC'),
    (r'Pixel Magic|LUMIN', 'Lumin'), (r'\bInnuos\b', 'Innuos'), (r'\bVolumio\b', 'Volumio'),
    (r'Raspberry Pi', 'Raspberry Pi'), (r'\bLinkplay\b', 'WiiM / Linkplay'), (r'\bZidoo\b|\bEversolo\b', 'Eversolo / Zidoo'),
    (r'\bTechnics\b', 'Technics'), (r'Panasonic', 'Panasonic'), (r'\bSamsung\b', 'Samsung'),
    (r'LG Electronics|LG Innotek', 'LG'), (r'\bSony\b', 'Sony'), (r'\bPhilips\b', 'Philips'),
    (r'\bHarman\b|JBL', 'Harman / JBL'), (r'\bBose\b', 'Bose'), (r'\bDynaudio\b', 'Dynaudio'),
    (r'\bMcIntosh\b', 'McIntosh'), (r'\bRoku\b', 'Roku'), (r'\bAmazon Technologies\b|\bAmazon\.com\b', 'Amazon'),
    (r'\bXiaomi\b', 'Xiaomi'), (r'\bDenafrips\b', 'Denafrips'), (r'\bHiFi\s*Rose\b|Citech', 'HiFi Rose'),
    (r'\bMatrix Audio\b', 'Matrix Audio'), (r'\bTOPPING\b', 'Topping'), (r'\bFiiO\b', 'FiiO'),
    (r'\bCary Audio\b', 'Cary Audio'), (r'Simaudio', 'Moon / Simaudio'), (r'\bEsoteric\b', 'Esoteric'),
    (r'\bAccuphase\b', 'Accuphase'), (r'\bLuxman\b', 'Luxman'), (r'\bAtoll\b', 'Atoll'),
    (r'StreamUnlimited', 'StreamUnlimited'), (r'\bLibre Wireless\b', 'Libre Wireless'), (r'\bFrontier Silicon\b', 'Frontier Silicon'),
]

rows = set()
for line in open(sys.argv[1]):
    if line.startswith('#') or not line.strip():
        continue
    parts = line.rstrip('\n').split('\t')
    if len(parts) < 3:
        continue
    prefix, _short, full = parts[0].strip(), parts[1].strip(), parts[2].strip()
    if '/' in prefix or prefix.count(':') != 2:
        continue
    for pat, name in BRANDS:
        if re.search(pat, full, re.I):
            rows.add((prefix.upper(), name))
            break

print('// Generated from the Wireshark manuf database (IEEE OUI registry),')
print('// filtered to audio/renderer brands. Regenerate with')
print('// scripts/gen_oui_audio.py against a fresh manuf file; sorted for')
print('// binary search.')
print('pub(crate) const OUI_AUDIO: &[(&str, &str)] = &[')
for p, n in sorted(rows):
    print(f'    ("{p}", "{n}"),')
print('];')
