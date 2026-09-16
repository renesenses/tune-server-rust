"""Peer CPace epingle, avec l'enveloppe SID de Sendspin/spec epinglee.

aiosendspin b6f8564 ne met pas encore round dans son _pake_sid.
Ce banc utilise CPace et ses helpers de code/wrapping, pas son parcours complet.
Tous les secrets sont des fixtures ephemeres tirees ici, jamais des comptes.
"""
import hashlib
import json
import secrets
import sys
from cpace import CPace, CPaceRole
from cryptography.hazmat.primitives.asymmetric.x25519 import X25519PrivateKey
from cryptography.hazmat.primitives.ciphers.aead import AESGCM, ChaCha20Poly1305
from aiosendspin.noise import pairing_code
from aiosendspin.noise.keys import b64url_encode, psk_id_for

for line in sys.stdin:
    q = json.loads(line)
    if q["op"] == "start":
        fmt, suite, scenario = q["format"], q["suite"], q["scenario"]
        h = bytes.fromhex(q["h"])
        sid = b"sendspin-pair-pake-v1" + h + q["index"].to_bytes(4,"big") + q["round"].to_bytes(4,"big")
        nb = pairing_code.generate_nonce()
        client_id = b64url_encode(X25519PrivateKey.generate().public_key().public_bytes_raw())
        result = {"commit": pairing_code.commit(nb).hex(), "client_id": client_id}
    elif q["op"] == "retry":
        sid = b"sendspin-pair-pake-v1" + h + q["index"].to_bytes(4,"big") + q["round"].to_bytes(4,"big")
        scenario = q.get("scenario", scenario)
        result = {"ready": True}
    elif q["op"] == "code":
        na = bytes.fromhex(q["nonce_a"])
        if fmt == "static":
            prs = b"01234567"
        elif fmt == "digits":
            prs = pairing_code.derive_digits(h,na,nb).encode()
        else:
            prs = pairing_code.derive_qr_code(h,na,nb)
        if scenario == "wrong_binding":
            prs = (("0" if prs[0] != 48 else "1").encode() + prs[1:]) if fmt=="digits" else bytes([prs[0]^1])+prs[1:]
        cpace = CPace.start(role=CPaceRole.RESPONDER, prs=prs, sid=sid, ad=b"client")
        result = {"code": prs.hex(), "share": cpace.public_share.hex()}
    elif q["op"] == "share":
        cpace.derive(bytes.fromhex(q["share"]), b"server")
        result = {"ready": True}
    elif q["op"] == "confirm":
        verified = cpace.verify(bytes.fromhex(q["tag"]))
        if scenario == "wrong_code":
            assert not verified, "reference must reject the server's wrong code"
            result = {"verified": False, "tag": cpace.tag().hex()}
        else:
            assert verified, "reference must verify native server confirmation"
            psk = secrets.token_bytes(32)
            cipher = AESGCM if suite == "aes" else ChaCha20Poly1305
            def wrap(label, value):
                key = hashlib.sha256(label+sid+cpace.isk).digest()
                return cipher(key).encrypt(bytes(12),value,b"")
            tag = cpace.tag()
            wp = wrap(b"sendspin-pair-psk-wrap-v1",psk)
            wn = wrap(b"sendspin-pair-nonce-wrap-v1",nb)
            if scenario == "wrong_tag": tag = bytes([tag[0]^1])+tag[1:]
            if scenario == "wrong_wrap": wp = bytes([wp[0]^1])+wp[1:]
            if scenario == "wrong_nonce_wrap": wn = bytes([wn[0]^1])+wn[1:]
            result = {"verified": True, "tag":tag.hex(), "psk_id":psk_id_for(psk), "psk":psk.hex(), "wrapped_psk":wp.hex(), "wrapped_nonce":wn.hex()}
    elif q["op"] == "end":
        print(json.dumps({"ended":True}),flush=True)
        break
    else:
        raise AssertionError("unknown operation")
    print(json.dumps(result),flush=True)
