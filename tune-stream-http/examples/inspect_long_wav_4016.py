import json, pathlib, struct, sys, tempfile, wave
root = pathlib.Path(sys.argv[1])
pcm = 6_359_040_000
truncated = False
for included in (False, True):
    header = (root / f"header-{str(included).lower()}.wav").read_bytes()
    with tempfile.TemporaryFile() as f:
        f.write(header)
        f.truncate(pcm + 44)
        f.seek(pcm + 44 - 8)
        f.write(b"FIN4016!")
        f.flush()
        f.seek(0)
        with wave.open(f, "rb") as reader:
            frames = reader.getnframes()
            reader.setpos(frames - 1)
            last = reader.readframes(2)
            eof = reader.readframes(1)
            row = dict(producer_header=included, parser="Python wave",
                       announced_frames=frames, announced_seconds=frames / reader.getframerate(),
                       frame_bytes=reader.getnchannels()*reader.getsampwidth(),
                       bytes_requested_at_last_frame=12, bytes_received=len(last),
                       eof_bytes=len(eof), physical_pcm_bytes=pcm)
            truncated |= frames * row["frame_bytes"] < pcm
            assert len(last) == 6 + struct.unpack_from("<I", header, 40)[0] % 6 and not eof, "le lecteur doit respecter la borne annoncée"
        f.seek(pcm+44-8)
        assert f.read() == b"FIN4016!", "le marqueur existe après la borne du lecteur"
        print(json.dumps(row))
    # Contrôle positif : le même lecteur atteint la dernière trame d'un
    # WAV court, dont l'en-tête décrit exactement les octets présents.
    short = bytearray(header)
    struct.pack_into("<I", short, 4, 72)
    struct.pack_into("<I", short, 40, 36)
    with tempfile.TemporaryFile() as f:
        f.write(short + bytes(30) + b"END401")
        f.seek(0)
        with wave.open(f, "rb") as reader:
            reader.setpos(reader.getnframes()-1)
            assert reader.readframes(2) == b"END401"
            assert reader.readframes(1) == b""
print('positive_control=PASS')

if truncated:
    print("container_check=FAIL: le lecteur WAV ne peut atteindre la fin du PCM annoncé par HTTP")
    sys.exit(2)
