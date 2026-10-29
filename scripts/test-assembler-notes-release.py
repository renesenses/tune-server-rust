#!/usr/bin/env python3
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent
SCRIPT = ROOT / 'scripts/assembler-notes-release.py'


class ReleaseNotesTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        for name in ['tune-server/src/i18n.rs', '.github/scripts/forme-des-notes.awk']:
            dest = self.root / name
            dest.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / name, dest)
        source = (self.root / 'tune-server/src/i18n.rs').read_text()
        declaration = re.search(r'pub const SUPPORTED:.*?=\s*&\[(.*?)\];', source, re.S)
        self.languages = re.findall(r'"([a-z]{2})"', declaration[1])
        self.notes = self.root / '.release/notes/v1.0.0-rc9'
        self.notes.mkdir(parents=True)
        for lang in self.languages:
            # Fixture data, not translations intended for publication.
            (self.notes / f'{lang}.md').write_text(f'## Fixes\n\n- Fixture {lang}.\n')

    def run_cli(self, *extra, tag='v1.0.0-rc9'):
        return subprocess.run([sys.executable, str(SCRIPT), '--root', str(self.root), '--tag', tag, *extra],
                              capture_output=True, text=True)

    def test_all_supported_languages_are_emitted_once_in_source_order(self):
        result = self.run_cli()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(re.findall(r'<!-- lang:(\w+) -->', result.stdout), self.languages)
        for lang in self.languages:
            self.assertIn(f'- Fixture {lang}.', result.stdout)

    def test_missing_translation_refuses_and_preserves_previous_output(self):
        language = self.languages[-1]
        (self.notes / f'{language}.md').unlink()
        output = self.root / 'previous.md'
        output.write_text('previous reviewed body')
        result = self.run_cli('--output', str(output))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(f'traduction absente : {language}', result.stderr)
        self.assertEqual(output.read_text(), 'previous reviewed body')
        self.assertFalse(list(self.root.glob('.release-notes-*')))

    def test_empty_prose_and_nested_markers_refuse(self):
        for text, reason in [('', 'vide'), ('## Playback\n\nOnly prose.', 'aucun item affichable'),
                             ('<!-- LANG:en -->\n- Extra block', 'marqueur')]:
            with self.subTest(reason=reason):
                (self.notes / 'fr.md').write_text(text)
                result = self.run_cli()
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(reason, result.stderr)

    def test_unknown_language_and_unsafe_tag_refuse(self):
        (self.notes / 'xx.md').write_text('- Unknown language')
        self.assertIn('langues non supportées', self.run_cli().stderr)
        self.assertNotEqual(self.run_cli(tag='../v1.0.0').returncode, 0)

    def test_new_server_language_is_required_without_changing_the_assembler(self):
        source = self.root / 'tune-server/src/i18n.rs'
        source.write_text(source.read_text().replace('pub const SUPPORTED: &[&str] = &[',
                                                  'pub const SUPPORTED: &[&str] = &["zz",'))
        result = self.run_cli()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('traduction absente : zz', result.stderr)

    def test_release_uses_assembled_body_and_ci_executes_this_target(self):
        workflow = (ROOT / '.github/workflows/release.yml').read_text()
        self.assertIn('python3 scripts/assembler-notes-release.py --tag "$GITHUB_REF_NAME" --output RELEASE_NOTES.md', workflow)
        self.assertIn('body_path: RELEASE_NOTES.md', workflow)
        self.assertNotIn('generate_release_notes: true', workflow)
        for path in ['scripts/assembler-notes-release.py', '.github/scripts/forme-des-notes.awk', 'tune-server/src/i18n.rs']:
            self.assertIn('            ' + path, workflow)
        self.assertIn('python3 scripts/test-assembler-notes-release.py', (ROOT / '.github/workflows/ci.yml').read_text())


if __name__ == '__main__':
    unittest.main()
