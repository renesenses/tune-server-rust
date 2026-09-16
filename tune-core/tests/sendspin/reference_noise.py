"""Peer using pinned aiosendspin, only for the explicit Shrek interop test.

Invoke through SENDSPIN_REFERENCE_PYTHON pointing to an isolated venv.
The Rust harness supplies test-only secrets; no product credentials are read.
"""
import json
import sys

from aiosendspin.noise.keys import Identity, b64url_encode, b64url_decode, psk_id_for
from aiosendspin.noise.models import NoiseMsg1Payload, NoiseMsg2Payload
from aiosendspin.noise.session import NoiseSession, NoiseCipherSuite

identity = Identity.generate()
session = None
previous = None
for line in sys.stdin:
    data = json.loads(line)
    op = data["op"]
    if op == "identity":
        result = {"id": identity.peer_id}
    elif op in ("start", "renew"):
        if op == "renew":
            previous = session
            prologue = previous.handshake_hash
            message = json.loads(previous.decrypt(b64url_decode(data["message"]))[1:])
        else:
            prologue = (data["client_init"] + data["server_init"]).encode()
            message = json.loads(data["message"])
        session = NoiseSession.as_responder(
            suite=NoiseCipherSuite(data["suite"]),
            local_static_priv=identity.private_bytes,
            remote_static_pub=b64url_decode(data["server_id"]),
            prologue=prologue,
        )
        payload = NoiseMsg1Payload.from_json(
            session.read_message(b64url_decode(message["payload"]["data"])).decode()
        )
        assert payload.psk_category == data["category"]
        assert payload.psk_id == psk_id_for(b64url_decode(data["advertised_psk"]))
        session.mix_psk(b64url_decode(data["used_psk"]))
        message_two = json.dumps({
            "type": "noise/handshake",
            "payload": {"data": b64url_encode(session.write_message(
                NoiseMsg2Payload().to_json().encode()))},
        })
        if op == "renew":
            message_two = b64url_encode(previous.encrypt(b"\x00" + message_two.encode()))
        result = {"message": message_two, "hash": b64url_encode(session.handshake_hash)}
    elif op == "transport":
        assert session.decrypt(b64url_decode(data["message"])) == b'\x00{"preuve":"Tune"}'
        result = {"message": b64url_encode(session.encrypt(b'\x00{"preuve":"aiosendspin"}'))}
    else:
        raise ValueError("unknown operation")
    print(json.dumps(result), flush=True)
