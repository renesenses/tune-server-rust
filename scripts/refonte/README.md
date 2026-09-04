# Outillage de la refonte du cœur (REF-0)

Quatre scripts, aucun ne touche au code. Ils servent à prouver qu'une PR de
découpage ou d'extraction n'a rien perdu : ni test, ni garde, ni signature.
Référence : epic #2219, registre des chantiers, code **REF-0**.

| Script | Ce qu'il relève | Coût |
|---|---|---|
| `gardes.sh [ref] [sortie]` | les tests qui relisent un fichier source par chemin (`include_str!`, `read_to_string`, `#[path]`), lecteur → fichier lu | `git grep`, immédiat |
| `tests-nominatifs.sh <dossier> [m1 m2 m3]` | le **nom** de chaque test, par matrice CI et par binaire, doctests compris | compile les trois matrices |
| `empreinte-api.sh <dossier> [ref]` | les signatures `pub` de tune-core et tune-server | `git grep`, immédiat |
| `comparer.sh <parent> <pr>` | tests disparus (bloquant), gardes déplacées (à lister), signatures disparues (bloquant en déplacement pur) | immédiat |

## Le rituel d'une PR

```bash
# 1. Le parent de la PR, pas une référence figée.
parent="$(git merge-base HEAD origin/main)"
scripts/refonte/gardes.sh        "$parent"  relevés/parent/gardes.txt
scripts/refonte/empreinte-api.sh relevés/parent "$parent"
git stash -u 2>/dev/null; git checkout -q "$parent"    # ou un second worktree
scripts/refonte/tests-nominatifs.sh relevés/parent
git checkout -q -; git stash pop 2>/dev/null

# 2. La tête de la PR.
scripts/refonte/gardes.sh        HEAD relevés/pr/gardes.txt
scripts/refonte/empreinte-api.sh relevés/pr
scripts/refonte/tests-nominatifs.sh relevés/pr

# 3. Le verdict, à coller dans la PR.
scripts/refonte/comparer.sh relevés/parent relevés/pr
```

Sur Shrek, avant toute commande cargo et à chaque session ssh :
`export CARGO_TARGET_DIR=/srv/cache/tune/targets/bertrand-<clé>` et
`export CARGO_BUILD_JOBS=6`. Le dossier `relevés/` est à suffixer par agent.

## Ce que ça ne prouve pas

Un `comparer.sh` vert est une contre-épreuve, pas une preuve. Les portes
complètes restent les matrices de `ci.yml`, les builds Windows/ASIO et macOS
pour tout lot touchant `outputs/local.rs`, et le workflow PostgreSQL si le
schéma bouge. Le raccourci `--features tune-server/oaat` ne compile pas la
sortie locale : il n'apparaît volontairement dans aucun script.
