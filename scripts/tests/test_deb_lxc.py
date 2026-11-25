"""Simulation du postinst et des sorties volatiles de systemd, sans privilèges."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

DEB = Path(__file__).resolve().parents[2] / "packaging/deb"


class Installation(unittest.TestCase):
    def test_detection_au_rechargement(self):
        with tempfile.TemporaryDirectory() as tmp:
            racine = Path(tmp)
            def commande(nom, corps):
                fichier = racine / nom
                fichier.write_text("#!/bin/sh\nset -eu\n" + corps + "\n")
                fichier.chmod(0o755)

            for nom in ("getent", "adduser", "addgroup", "install", "chown"):
                commande(nom, "exit 0")
            commande("systemd-detect-virt", 'test "$1" = --container; echo "$VIRT"; exit "$DETECT"')
            commande("unshare", 'test "$*" = "--mount --propagation slave /bin/true"; exit "$MOUNT"')
            commande("systemctl", """echo "$*" >> "$LOG"
if [ "$1" = daemon-reload ]; then
    rm -rf "$OUT"
    mkdir "$OUT"
    "$GENERATEUR" "$OUT"
fi""")
            postinst = (DEB / "postinst").read_text().replace("/var/lib/tune", tmp + "/data").replace("/run/systemd/system", tmp)
            sortie = racine / "sortie"
            surcharge = sortie / "tune-server.service.d/20-tune-lxc.conf"
            log = racine / "log"
            env = dict(os.environ, PATH=tmp + ":" + os.environ["PATH"], OUT=str(sortie),
                       GENERATEUR=str(DEB / "tune-lxc-generator"), LOG=str(log))
            for virt, detect, montage, attendu, version in [
                ("lxc", 0, 1, True, ""), ("lxc", 0, 1, True, "1"),
                ("none", 1, 1, False, "1"), ("docker", 0, 1, False, "1"),
                ("lxc-libvirt", 0, 1, True, "1"), ("lxc", 0, 0, False, "1"),
            ]:
                with self.subTest(virt=virt, montage=montage):
                    log.write_text("")
                    subprocess.run(["sh", "-s", "configure", version], input=postinst,
                                   text=True, check=True, stdout=subprocess.DEVNULL,
                                   env=dict(env, VIRT=virt, DETECT=str(detect), MOUNT=str(montage)))
                    self.assertEqual(log.read_text().splitlines(),
                                     ["daemon-reload", "enable tune-server.service",
                                      ("try-restart" if version else "start") + " tune-server.service"])
                    self.assertEqual(surcharge.exists(), attendu)
                    if attendu:
                        self.assertEqual(surcharge.read_text().splitlines(),
                                         ["[Service]", "ProtectSystem=no", "PrivateTmp=no"])
            unite = (DEB / "tune-server.service").read_text().splitlines()
            for ligne in ("ProtectSystem=full", "PrivateTmp=yes", "NoNewPrivileges=yes"):
                self.assertIn(ligne, unite)


if __name__ == "__main__":
    unittest.main()
