# Copie d'albums : dates de fichiers tronquées (#5223)

## Défaut isolé

Les deux parcours de lecture (`scan_files_parallel`, `scan_files_batched`)
tronquaient la date de modification à la seconde. La ligne `tracks` héritait
de cette date ; les gardes du surveillant, du préfiltre des scans et du verdict
d'écriture arrondissaient également et toléraient 500 ms d'écart.

Un fichier préalloué peut finir sa copie à taille égale, dans la même seconde.
Si sa première lecture utilise les noms de dossier/fichier faute de balises
lisibles, les notifications finales et l'analyse rapide peuvent conserver ce
repli. Le scan complet contourne ces gardes. Une retouche de balises de même
longueur subit le même défaut.

Cela explique un scénario compatible avec le signalement, sans établir que
la copie du testeur a suivi ce calendrier. Les noms exacts de ses sous-dossiers,
la méthode de copie et ses dates sur disque ne sont pas connus.

## Correction

Conserver `Duration::as_secs_f64()` depuis le stat jusqu'à `file_mtime` et
comparer les dates sans tolérance. Les colonnes SQLite REAL et PostgreSQL
DOUBLE PRECISION sont déjà adaptées ; le chemin CUE utilise déjà ce format.
Les copies dédupliquées héritent de la même date par leur ligne de piste.
Aucune migration ni modification du regroupement multi-disques.

Une ancienne date tronquée provoque une relecture si la date précise du
fichier diffère. Cela peut rendre la première analyse rapide plus longue sur
une grande bibliothèque/NAS. Les passages suivants retrouvent leur raccourci.
Les dates réellement entières (système de fichiers moins précis) restent
comparées à l'identique.

## Banc Shrek

`tune-server/src/copie_albums_tests_5223.rs` crée deux FLAC balisés sous CD1 et
CD2, une base SQLite sur disque et utilise les vrais gestionnaires du serveur.
La seconde piste est d'abord un fichier préalloué de zéros, de taille finale,
avec mtime = 1750000000,125 s. Le surveillant l'importe avec le repli. Le banc
rétablit les octets FLAC sans changer la taille et pose mtime = 1750000000,250 s.
Les dates sont imposées, pas obtenues par une course entre threads.

Six passages : rapide, démarrage, surveillant, rapide sur une ancienne ligne
datée à la seconde, complet témoin, surveillant avec un dossier nommé
« Coffret copie » et une balise ALBUM « Double album ». Dans ce dernier cas,
le fichier incomplet arrive en premier et crée réellement un album au nom du
dossier ; la reprise doit en corriger le titre. Chaque passage vérifie le vrai titre,
l'appartenance au même album que CD1, puis l'absence de réimport sur événement
inchangé et le retour du préfiltre rapide. Le repli initial inchangé est aussi
protégé contre une boucle du surveillant.

```sh
cargo test -p tune-server --lib --no-default-features --features oaat 5223 -- --nocapture --test-threads=1
```

Les résultats de référence, après correction et de la contre-épreuve sont
consignés dans la PR liée à #5223. Contre-épreuve : rétablir uniquement la
tolérance de 500 ms dans les trois gardes de `scan.rs` et `auto_scan.rs`,
conserver tous les tests et les types, recompiler, puis restaurer ces deux
sources par copie et rejouer exactement le même banc.

## Limites

Pas d'essai sur le PC Windows ni sur les fichiers de Didier. Ce correctif ne
prouve pas la fin d'une copie après une période stable et ne détecte pas un
changement dont la taille **et** la date précise sont identiques. Il ne relit
pas toutes les balises sur chaque événement : le garde contre les boucles
reste actif. Aucun merge, bump ou déploiement dans cette intervention.
