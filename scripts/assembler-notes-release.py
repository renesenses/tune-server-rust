#!/usr/bin/env python3
"""Assemble reviewed per-language release notes; never translate or publish."""
import argparse
from pathlib import Path
import re
import subprocess
import tempfile
import os


def assemble(root, tag):
    if not re.fullmatch(r'v[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?', tag):
        raise ValueError('tag attendu : vX.Y.Z ou vX.Y.Z-prerelease')
    source = (root / 'tune-server/src/i18n.rs').read_text(encoding='utf-8')
    declaration = re.search(r'pub const SUPPORTED:\s*&\[&str\]\s*=\s*&\[(.*?)\];', source, re.S)
    if declaration is None:
        raise ValueError('langues SUPPORTED introuvables dans i18n.rs')
    languages = re.findall(r'"([a-z]{2})"', declaration[1])
    if not languages or len(set(languages)) != len(languages):
        raise ValueError('liste SUPPORTED vide ou dupliquée')
    directory = root / '.release/notes' / tag
    unexpected = {p.stem for p in directory.glob('*.md')} - set(languages)
    if unexpected:
        raise ValueError('langues non supportées : ' + ', '.join(sorted(unexpected)))
    blocks = []
    for language in languages:
        path = directory / f'{language}.md'
        if not path.is_file():
            raise ValueError(f'traduction absente : {language} ({path})')
        body = path.read_text(encoding='utf-8').strip()
        if not body:
            raise ValueError(f'traduction vide : {language}')
        if re.search(r'<!--\s*lang\s*:', body, re.I):
            raise ValueError(f'marqueur de langue interdit dans le fichier : {language}')
        # Reuse the guarded reader, rather than inventing a second Markdown grammar.
        result = subprocess.run(['awk', '-f', str(root / '.github/scripts/forme-des-notes.awk')],
                                input=body, text=True, capture_output=True, check=True)
        counts = dict(line.split('\t', 1) for line in result.stdout.splitlines())
        if sum(int(counts[key]) for key in ('features', 'improvements', 'fixes')) == 0:
            raise ValueError(f'aucun item affichable : {language}')
        blocks.append(f'<!-- lang:{language} -->\n\n{body}')
    return '\n\n'.join(blocks) + '\n'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path, default=Path(__file__).resolve().parent.parent)
    parser.add_argument('--tag', required=True)
    parser.add_argument('--output', type=Path)
    args = parser.parse_args()
    try:
        body = assemble(args.root, args.tag)
        if args.output is None:
            print(body, end='')
        else:
            # Validate every translation before touching a previous output.
            with tempfile.NamedTemporaryFile(mode='w', encoding='utf-8', dir=args.output.parent,
                                             prefix='.release-notes-', delete=False) as output:
                temporary = Path(output.name)
                try:
                    output.write(body)
                    output.flush()
                    os.fsync(output.fileno())
                except BaseException:
                    temporary.unlink(missing_ok=True)
                    raise
            try:
                os.replace(temporary, args.output)
            finally:
                temporary.unlink(missing_ok=True)
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        parser.exit(1, f'Notes de version refusées : {error}\n')


if __name__ == '__main__':
    main()
