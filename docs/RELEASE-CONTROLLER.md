# Contrôleur de release

Le fichier `.release/vX.Y.Z.json` épingle les quatre sources d'un train. Le
SHA serveur vaut `self` dans le plan, puis le contrôleur le remplace par le SHA
exact de `main` qui exécute le workflow.

Un manifeste peut rester en préparation avec `ready: false`. Le dry-run le
contrôle, mais aucune création réelle de tag n'est possible avant le passage
explicite à `ready: true` avec les SHA finaux.

Le workflow `Release controller` vérifie que chaque SHA est atteignable depuis
le `main` de son dépôt. Il crée ensuite, de manière idempotente :

1. `vX.Y.Z` dans `tune-web-client` ;
2. `vX.Y.Z` dans `tune-server-universal` ;
3. `tune-os-rpi-vX.Y.Z` dans `tune-os` ;
4. `vX.Y.Z` dans `tune-server-rust`, en dernier.

Un tag existant sur un autre SHA bloque le train ; aucun tag n'est déplacé ou
supprimé. Le mode dry-run est actif par défaut.

Pour armer la création réelle, l'environnement GitHub `release` doit être
protégé, la variable `RELEASE_CONTROLLER_ENABLED` doit valoir `true`, et le
secret `RELEASE_CONTROLLER_TOKEN` doit appartenir à l'identité dédiée prévue
par #2814. Les agents de correctif ne détiennent pas ce secret.

Le contrôleur ne doit être armé qu'après retrait des publications indépendantes
sur `push tag`. Le tag serveur reste l'unique signal du train ; les trois autres
tags ne servent qu'à l'identité et à la traçabilité des composants.

## Secrets et environnements

- `release` : `RELEASE_CONTROLLER_TOKEN` et approbation, avec
  `RELEASE_CONTROLLER_ENABLED=true` seulement pendant la pose des tags ;
- dépôt Tune OS : `TUNE_SERVER_RELEASE_TOKEN`, lecture seule sur les releases
  serveur, pour récupérer les tarballs encore en brouillon ;
- staging serveur : `TUNE_OS_DISPATCH_TOKEN`, `DOCKERHUB_USERNAME`,
  `DOCKERHUB_TOKEN` et la clé minisign ;
- `release-promotion` : les mêmes accès plus `HOMEBREW_TAP_TOKEN` et
  `WEBHOOK_SECRET`, avec `RELEASE_PROMOTION_ENABLED=true` seulement pendant la
  promotion ;
- `release-dry-run` : mêmes accès en lecture, sans pouvoir de publication.

Le workflow de promotion refuse un manifeste non prêt, un tag divergent, une
release déjà publique en dry-run, une signature absente, un actif OS manquant
ou un digest Docker staged absent. Une relance en mode réel accepte en revanche
les canaux déjà promus sur le même train et termine les étapes restantes.
