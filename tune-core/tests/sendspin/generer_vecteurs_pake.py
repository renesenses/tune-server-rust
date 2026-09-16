"""Regenerer les vecteurs CPace derives de la reference, sans employer Tune.

Usage : venv/bin/python3 tune-core/tests/sendspin/generer_vecteurs_pake.py OUTPUT
Requires cpace==0.1.0 and aiosendspin b6f8564d07b212d77bfb026b80baa23435d9e591.
Les vecteurs du brouillon et les 129 generateurs ont leur provenance propre.
"""
from pathlib import Path
import hashlib
import importlib.metadata
import json
import sys
from cpace import CPace, CPaceRole, _calculate_generator, _scalar_mult_vfy
from cryptography.hazmat.primitives.ciphers.aead import AESGCM, ChaCha20Poly1305
from cryptography.hazmat.primitives.asymmetric.x25519 import X25519PrivateKey
from aiosendspin.noise import pairing_code
from aiosendspin.noise.keys import b64url_encode

assert importlib.metadata.version("cpace") == "0.1.0"
destination = Path(sys.argv[1])
destination.mkdir(parents=True, exist_ok=True)

def sha(label):
    return hashlib.sha256(label.encode()).digest()

def run(prs, ci, sid, ada, adb, sa, sb):
    g = _calculate_generator(prs, ci, sid)
    ya, yb = _scalar_mult_vfy(sa, g), _scalar_mult_vfy(sb, g)
    a = CPace(role=CPaceRole.INITIATOR, sid=sid, ad=ada, scalar=sa, public_share=ya)
    b = CPace(role=CPaceRole.RESPONDER, sid=sid, ad=adb, scalar=sb, public_share=yb)
    a.derive(yb, adb)
    b.derive(ya, ada)
    assert a.isk == b.isk and a.verify(b.tag()) and b.verify(a.tag())
    return a, b, dict(prs=prs, ci=ci, sid=sid, ada=ada, adb=adb, sa=sa, sb=sb,
                     ya=ya, yb=yb, isk=a.isk, ta=a.tag(), tb=b.tag())

generic = []
for i in range(64):
    n = [0, 1, 6, 8, 24, 126, 127, 128, 129, 255, 256, 1024][i % 12]
    prs = (sha(f"prs{i}") * 33)[:n]
    ci = (sha(f"ci{i}") * 9)[:[0, 127, 128, 256][i % 4]]
    _, _, c = run(prs, ci, sha(f"sid{i}"), b"server", b"client", sha(f"a{i}"), sha(f"b{i}"))
    generic.append({k: x.hex() for k, x in c.items()})

flows = []
for fmt in ["static", "digits", "qr"]:
    for suite in ["chacha", "aes"]:
        h, na, nb = sha(fmt + "h"), sha(fmt + "a"), sha(fmt + "b")
        index, tour = 3, 1
        # La spec epinglee contient round ; le parcours aiosendspin epingle ne l'a pas encore.
        sid = b"sendspin-pair-pake-v1" + h + index.to_bytes(4, "big") + tour.to_bytes(4, "big")
        prs = (b"01234567" if fmt == "static" else
               pairing_code.derive_digits(h, na, nb).encode() if fmt == "digits" else
               pairing_code.derive_qr_code(h, na, nb))
        _, b, c = run(prs, b"", sid, b"server", b"client", sha(fmt + "scalar_a"), sha(fmt + "scalar_b"))
        psk = sha(fmt + suite + "long_term_psk")
        cipher = ChaCha20Poly1305 if suite == "chacha" else AESGCM
        def wrap(label, value):
            return cipher(hashlib.sha256(label + sid + b.isk).digest()).encrypt(bytes(12), value, b"")
        c.update(h=h, nonce_a=na, nonce_b=nb, commit_b=pairing_code.commit(nb), psk=psk,
                 wrapped_psk=wrap(b"sendspin-pair-psk-wrap-v1", psk),
                 wrapped_nonce=wrap(b"sendspin-pair-nonce-wrap-v1", nb))
        entry = {k: x.hex() for k, x in c.items()}
        entry.update(format=fmt, suite=suite, index=index, tour=tour,
                     client_id=b64url_encode(X25519PrivateKey.from_private_bytes(
                         sha("clientstatic")).public_key().public_bytes_raw()))
        flows.append(entry)

for name, value in [("reference-vectors.json", generic), ("flow-vectors.json", flows)]:
    data = (json.dumps(value, indent=2) + "\n").encode()
    (destination / name).write_bytes(data)
    print(hashlib.sha256(data).hexdigest(), name)
