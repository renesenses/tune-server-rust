"""Vecteurs d'entree operateur generes par aiosendspin epingle, hors runtime Tune."""
import base64
import json
import sys
from aiosendspin.noise.pairing_token import decode_pairing_code_token, decode_psk_token

cases = []
for version, size in [(0, 64), (1, 24)]:
    for length in [0, 1, size - 2, size - 1, size, size + 1, size + 2, size + 3, size + 4, size + 5, size + 17, size + 256]:
        raw = bytes((n * 37 + 11) % 256 for n in range(length))
        body = base64.b32encode(raw).decode().rstrip("=").replace("2", "9")
        token = f"SP:{version}{body}"
        try:
            if version == 0:
                value = decode_psk_token(token)
                payload = base64.urlsafe_b64decode(value.client_id + "=") + value.pairing_psk
            else:
                payload = decode_pairing_code_token(token)
            cases.append(dict(version=version, token=token, valid=True, payload=list(payload)))
        except ValueError:
            cases.append(dict(version=version, token=token, valid=False))
with open(sys.argv[1], "w") as output:
    json.dump(cases, output, indent=2)
    output.write("\n")
