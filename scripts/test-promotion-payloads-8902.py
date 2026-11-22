"""Exécute le vrai bloc des webhooks avec gh/curl factices, jamais le réseau."""
import hashlib
import hmac
import json
import os
from pathlib import Path
import subprocess
import tempfile
import textwrap
import unittest

ROOT = Path(__file__).resolve().parents[1]

class PayloadsPromotion(unittest.TestCase):
    def execute(self, os_present=True, curl_failure=False, server_size=200 * 1024):
        workflow = (ROOT / '.github/workflows/promote-release.yml').read_text()
        start = workflow.index('          [ -n "$WEBHOOK_SECRET" ]')
        end = workflow.index('          {\n            echo "## v$VERSION promue"', start)
        script = 'set -euo pipefail\n' + textwrap.dedent(workflow[start:end])
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            server = {'tag_name': 'vtest', 'body': 'x' * server_size + '\n"été"\\fin', 'assets': []}
            os_release = {'tag_name': 'ostest', 'body': 'z' * (200 * 1024) + '\n"OS"', 'assets': []}
            (root / 'server.json').write_text(json.dumps(server))
            (root / 'os.json').write_text(json.dumps([os_release] if os_present else []))
            (root / 'gh').write_text('#!/usr/bin/env python3\nimport os,sys\nfrom pathlib import Path\nname="server.json" if any("releases/tags/" in a for a in sys.argv) else "os.json"\nprint((Path(os.environ["FIXTURE"])/name).read_text())\n')
            (root / 'curl').write_text(r"""#!/usr/bin/env python3
import os,sys,json,hashlib,hmac
from pathlib import Path
if os.environ.get('CURL_FAILURE') == '1': sys.exit(22)
a=sys.argv[1:]
flag='--data-binary' if '--data-binary' in a else '-d'
payload=a[a.index(flag)+1]
body=Path(payload[1:]).read_bytes() if payload.startswith('@') else payload.encode()
headers=[a[i+1] for i,x in enumerate(a) if x=='-H']
expected='X-Hub-Signature-256: sha256='+hmac.new(os.environ['WEBHOOK_SECRET'].encode(),body,hashlib.sha256).hexdigest()
assert expected in headers, 'signature sur des octets différents du corps'
obj=json.loads(body)
with (Path(os.environ['FIXTURE'])/'captured.jsonl').open('a') as f: f.write(json.dumps(obj)+'\n')
""")
            for name in ['gh', 'curl']: (root/name).chmod(0o755)
            env = dict(os.environ, PATH=str(root)+os.pathsep+os.environ['PATH'], FIXTURE=str(root), RUNNER_TEMP=str(root), TAG='vtest', OS_TAG='ostest', OS_TOKEN='factice', GITHUB_REPOSITORY='renesenses/tune-server-rust', WEBHOOK_SECRET='cle-factice-pour-test', CURL_FAILURE='1' if curl_failure else '0')
            result = subprocess.run(['bash', '-c', script], env=env, text=True, capture_output=True)
            captured = root/'captured.jsonl'
            bodies = [json.loads(s) for s in captured.read_text().splitlines()] if captured.exists() else []
            return result, bodies, server, os_release

    def test_payloads_200_kio_et_signatures_8902(self):
        result, bodies, server, os_release = self.execute()
        self.assertEqual(result.returncode, 0, 'les notes de 200 Kio doivent passer sans argument trop long: '+result.stderr)
        self.assertEqual([b['release'] for b in bodies], [server, os_release])
        self.assertEqual([b['repository']['full_name'] for b in bodies], ['renesenses/tune-server-rust','renesenses/tune-os'])
        self.assertTrue(all(b['action']=='published' for b in bodies))

    def test_os_seul_a_200_kio_8902(self):
        result, bodies, server, os_release = self.execute(server_size=32)
        self.assertEqual(result.returncode, 0, 'le rejeu OS doit accepter 200 Kio: '+result.stderr)
        self.assertEqual([b['release'] for b in bodies], [server, os_release])

    def test_os_absent_garde_le_webhook_serveur_8902(self):
        result, bodies, server, _ = self.execute(os_present=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([b['release'] for b in bodies], [server])
        self.assertIn('introuvable au moment du rejeu', result.stdout)

    def test_refus_http_reste_bloquant_8902(self):
        result, bodies, _, _ = self.execute(curl_failure=True, server_size=32)
        self.assertEqual(result.returncode, 22, result.stderr)
        self.assertEqual(bodies, [])

if __name__ == '__main__': unittest.main()
