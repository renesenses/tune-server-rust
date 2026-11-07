# Choix d’un flux radio selon le pays

Demande : https://github.com/renesenses/tune-server-rust/issues/7199.
Base assignée : `batch/bugs-rc9-20261025`.

## Comportement

Le serveur conserve `restricted_to_countries` pour chaque flux publié de
l’annuaire v2. Il ne déduit jamais le pays de l’auditeur de la langue du
navigateur, du pays de la station ou d’un palmarès musical.

Le réglage serveur `radios_country` se lit et s’écrit par `/api/v1/system/config` :
`PATCH {"radios_country":"FR"}`. Deux lettres sont normalisées en majuscules ;
`null`, une chaîne vide ou `auto` signifient « pays inconnu ». Le code mondial
`ZZ`, qui n’est pas un pays d’écoute, est refusé. Aucun écran web n’est modifié.

Ordre de choix parmi les flux autorisés :

1. Pour une station enregistrée, son adresse préférée si elle reste autorisée.
2. Le flux principal de la fiche, s’il est autorisé.
3. Un flux sans perte, puis le débit et la fréquence les plus élevés ; l’ordre
   de publication départage les ex æquo.

Sans pays connu, seuls les flux sans restriction sont retenus. Une restriction
absente, `null` ou `[]` indique un flux sans restriction. Un champ mal formé
n’ouvre pas l’accès. Un flux non publié et un alias historique v1 ne servent pas
à contourner la liste détaillée v2, même si celle-ci est vide.

Le choix est effectué à l’ajout depuis l’annuaire et à chaque lecture : fiche,
« Mes radios », corps audio du serveur multimédia, commande générique d’une
zone et reprise passant par cette commande. Une écoute ne réécrit pas la
préférence enregistrée. Une station personnelle sans fiche garde son URL.
Une fiche sans flux accessible peut toujours être ajoutée en favori : seule
sa lecture est refusée.

Quand aucun flux n’est autorisé, les routes radio rendent HTTP 422,
`output_sent:false`, `error_code:"radio_country_unavailable"` et un message
lisible. Le chemin générique des zones conserve son enveloppe d’erreur
habituelle et rend le même message. Le refus radio précède tout démarrage
et toute incrémentation du compteur d’écoutes.

## Essai ciblé

Les témoins sont dans `radios_geofencing_7199_tests.rs`, module de la bibliothèque
`tune-server` : ils sont réellement compilés malgré `autotests=false`.
Ils couvrent le filtrage FR/GB/US, le pays absent, le choix préféré, les anciens
alias, les flux non publiés, les restrictions absentes ou mal formées, les
changements de réglage, le lien stable d’une radio enregistrée, l’ajout et les
refus par les véritables routes HTTP. Aucune sortie sonore n’est utilisée.

## Ce qui reste ouvert

Suivi : https://github.com/renesenses/tune-server-rust/issues/7246.

Les données de ces témoins sont **synthétiques**. Aucune fiche réelle portant
des restrictions géographiques n’a été remise à cette tentative. L’extrait réel
déjà présent dans le dépôt ne contient que des restrictions nulles.
La capture anonymisée d’une fiche réellement restreinte et l’essai du vrai
annuaire et d’un vrai flux depuis le .18 restent à faire avec les accès adaptés.
Ils ne sont pas présentés comme couverts par les doublures locales.

## Contre-épreuve exécutée

### Reproduction indépendante

La reproduction exige un espace autorisant les écritures temporaires : source,
sauvegardes et sorties de compilation. Un clone strictement en lecture ne
permet pas cette opération. Une nouvelle exécution par le développeur ne
remplace pas celle du testeur ; l'autorisation de son espace doit être réglée
avant l'essai. Aucun changement de droits n'est effectué par cette procédure.

Depuis la racine d'un clone propre du candidat, exécuter le bloc suivant avec
Bash. Il conserve le test intact, remplace seulement le prédicat de production,
exige l'échec de l'assertion attendue après compilation, puis restaure le fichier
par copie et rejoue le même test. Les journaux et sauvegardes restent dans le
répertoire indiqué à la fin. Ne faire ni commit ni poussée pendant cet essai.

```bash
set -euo pipefail
test -z "$(git status --porcelain)"
proof_dir=$(mktemp -d "$(git rev-parse --git-dir)/contre-epreuve-7199.XXXXXX")
source_file=tune-server/src/routes/radios_geofencing_7199.rs
test_file=tune-server/src/routes/radios_geofencing_7199_tests.rs
cp "$source_file" "$proof_dir/source.rs"
cp "$test_file" "$proof_dir/tests.rs"
trap 'cp "$proof_dir/source.rs" "$source_file"' EXIT

run_witness() {
    CARGO_BUILD_JOBS=4 cargo test -p tune-server --no-default-features --lib \
        country_filter_rejects_the_foreign_primary_7199 -- \
        --exact routes::radios::geofencing::tests::country_filter_rejects_the_foreign_primary_7199
}

run_witness > "$proof_dir/avant.log" 2>&1
python3 - <<'PY'
from pathlib import Path

path = Path("tune-server/src/routes/radios_geofencing_7199.rs")
source = path.read_text()
start = source.index("        .filter(|f| {")
end = source.index("        .max_by_key(|f| {", start)
assert source.count("        .filter(|f| {") == 1
path.write_text(source[:start] + "        .filter(|_| true)\n" + source[end:])
PY
cmp "$test_file" "$proof_dir/tests.rs"
red_status=0
run_witness > "$proof_dir/rouge.log" 2>&1 || red_status=$?
test "$red_status" -eq 101
rg -F 'test result: FAILED. 0 passed; 1 failed;' "$proof_dir/rouge.log"
rg -F 'le flux principal réservé au Royaume-Uni doit être écarté en France' "$proof_dir/rouge.log"
rg -F 'left: "https://radio.example/gb.m3u8"' "$proof_dir/rouge.log"
rg -F 'right: "https://radio.example/fr.aac"' "$proof_dir/rouge.log"

cp "$proof_dir/source.rs" "$source_file"
cmp "$source_file" "$proof_dir/source.rs"
cmp "$test_file" "$proof_dir/tests.rs"
run_witness > "$proof_dir/apres.log" 2>&1
rg -F 'test result: ok. 1 passed; 0 failed;' "$proof_dir/apres.log"
test -z "$(git status --porcelain)"
trap - EXIT
printf 'Journaux et sauvegardes : %s\n' "$proof_dir"
```

Le test `empty_unpublished_or_malformed_v2_never_reopens_legacy_7199`
couvre déjà explicitement `streams: []` ; aucun nouveau témoin ni changement
de sélection n'est nécessaire pour ce cas.

### Exécution initiale du développeur

Après un premier passage vert des 70 tests radio ciblés (dont les 11 nouveaux),
le prédicat de sélection des flux a été forcé à `true`, dans le code de
production seulement. Aucun témoin n’a été modifié.

Commande exécutée :

```sh
CARGO_BUILD_JOBS=4 cargo test -p tune-server --no-default-features --lib country_filter_rejects_the_foreign_primary_7199 -- --exact routes::radios::geofencing::tests::country_filter_rejects_the_foreign_primary_7199
```

Résultat : compilation réussie, **1 test exécuté, 1 échec**, celui attendu :

```text
assertion `left == right` failed: le flux principal réservé au Royaume-Uni doit être écarté en France
  left: "https://radio.example/gb.m3u8"
 right: "https://radio.example/fr.aac"
```

Le module corrigé a ensuite été restauré par `cp` depuis sa sauvegarde ;
`cmp` a confirmé la restauration à l’identique.

## Retour au vert et formatage

```sh
CARGO_BUILD_JOBS=4 cargo test -p tune-server --no-default-features --lib routes::radios -- --test-threads=2
```

Après restauration : `70 passed; 0 failed; 0 ignored; 2671 filtered out`.
Ce filtre couvre les radios, leur annuaire et leurs libellés, dont les
11 témoins de cette correction. Les autres tests du dépôt n’ont pas été lancés.

```sh
cargo fmt -p tune-server -- --check --config skip_children=true tune-server/src/routes/radios.rs tune-server/src/routes/radios_annuaire.rs tune-server/src/routes/radios_geofencing_7199.rs tune-server/src/routes/radios_geofencing_7199_tests.rs tune-server/src/routes/system/config.rs tune-server/src/routes/playback.rs
```

Résultat : code de sortie 0, aucune sortie. Le formatage porte sur les fichiers
Rust modifiés ; les modules enfants ne sont pas parcourus.

## Analyse statique ciblée

```sh
CARGO_BUILD_JOBS=4 cargo clippy -p tune-server --no-default-features --lib --no-deps
```

Résultat : code de sortie 0, `Finished dev profile` en 4 min 27 s.
Clippy signale 85 avertissements pour la bibliothèque serveur, aucun dans le
nouveau module de sélection. Ils ne sont pas corrigés hors périmètre ici.
La CI contrôle les configurations livrées et l’audit de sécurité ; aucune suite
complète ni audit de sécurité n’ont été lancés localement.
