# Mesurer le graphe de compilation Rust

Les optimisations de compilation sont comparées avec une commande versionnée,
sur le même commit et le même profil Cargo. La commande produit un inventaire
JSON, un résumé Markdown, les journaux complets et les rapports HTML de
`cargo --timings`.

## Inventaire sans compilation

```sh
python3 scripts/measure-compile.py inventory
```

Cette passe relève les crates locales, les lignes Rust, les dépendances
directes et les harnais de tests. Elle ne lance aucun build.

## Profils mesurés

```sh
# Commande exacte du job Linux Test, deux fois : target froid puis chaud.
python3 scripts/measure-compile.py ci-test

# Commande exacte du job Clippy, deux fois.
python3 scripts/measure-compile.py ci-clippy

# Build de livraison natif correspondant au système hôte, froid puis chaud.
python3 scripts/measure-compile.py release
```

Par défaut, chaque profil de compilation crée son propre `target` avec
`mkdtemp`, l’utilise pour les deux passes, copie les rapports, puis supprime
uniquement ce répertoire temporaire. Cela évite de présenter un cache étranger
comme un build froid et évite aussi de recréer les dizaines de `target` qui ont
occupé 298 Gio pendant la passe backlog.

Pour mesurer volontairement la réutilisation entre plusieurs commandes :

```sh
python3 scripts/measure-compile.py ci-test --target-dir /chemin/explicite
python3 scripts/measure-compile.py ci-clippy --target-dir /chemin/explicite
```

Le chemin doit être explicite. La commande ne nettoie jamais un `target`
fourni par l’appelant. `--keep-target` conserve au contraire un target
temporaire afin de permettre une inspection manuelle.

## Comparaison avant/après

Une PR de découpage publie au minimum :

- le commit, le système, la cible et la version de Rust enregistrés dans le
  rapport ;
- les durées froides et chaudes du même profil ;
- le nombre de harnais liés et la taille finale du `target` ;
- les deux rapports `cargo-timing-*.html` lorsqu’une compilation a eu lieu.

Une mesure locale ne remplace pas la durée observée dans GitHub Actions. Elle
sert à expliquer le coût et à comparer deux graphes dans un environnement
identique ; la batterie complète du lot confirme ensuite macOS, Windows et les
fonctionnalités livrées.
