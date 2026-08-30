# Contrôleur de release

Le fichier `.release/vX.Y.Z.json` épingle les quatre sources d'un train. Le
SHA serveur vaut `self` dans le plan, puis le contrôleur le remplace par le SHA
exact de `main` qui exécute le workflow.

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
