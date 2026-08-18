# Signature des plugins marketplace

Contrat entre le serveur Tune et le service marketplace (`mozaiklabs`, dépôt
séparé). Le client est implémenté ; **la moitié serveur reste à faire**, et
tant qu'elle n'existe pas la vérification ne peut rien vérifier.

## Pourquoi

Un plugin WASM s'exécute **dans** le processus `tune-server`. Un marketplace
compromis — ou un intermédiaire capable de répondre à sa place — qui pousse un
artefact malveillant obtient donc l'exécution de code sur la machine de
l'utilisateur, avec les droits du serveur : accès à la bibliothèque, à la base,
aux tokens streaming.

Le téléchargement est borné en taille depuis #1177, mais rien n'authentifiait
l'artefact (audit item 8). C'est la seule partie de la chaîne de livraison qui
ne l'était pas : la mise à jour du serveur, elle, est signée en minisign et
vérifiée depuis la même vague de remédiation.

## Ce que le marketplace doit exposer

Pour chaque artefact servi par :

```
GET /api/v1/plugins/{name}/download
```

servir la signature détachée correspondante par :

```
GET /api/v1/plugins/{name}/download.minisig
```

Contraintes :

- **Signature minisign détachée**, calculée sur **les octets exacts** de
  l'artefact renvoyé par la route `download` — pas sur une archive
  recompressée, pas sur un manifeste. Toute recompression côté serveur invalide
  la signature.
- Format non préhaché (`minisign -S`, ce que produit la CLI par défaut). Le
  client appelle `verify(bytes, sig, false)`.
- `404` si l'artefact n'est pas signé. Le client distingue « pas de signature »
  (404) d'une « erreur de transport » (5xx, timeout) et ne traite jamais la
  seconde comme la première — sinon quiconque peut couper la connexion peut
  supprimer la vérification.
- La signature doit être régénérée à chaque nouvelle version publiée.

## Clé

Générer une paire **dédiée aux plugins** :

```bash
minisign -G -p plugins.pub -s plugins.key
```

- La clé secrète va dans les secrets CI du dépôt marketplace, et **nulle part
  ailleurs**.
- La ligne base64 de `plugins.pub` va dans `PLUGIN_PUBLIC_KEY`
  (`tune-server/src/routes/marketplace.rs`).

Ne **pas** réutiliser la clé de mise à jour du serveur. Les deux chaînes ont
des surfaces de compromission et des cadences de rotation différentes ; un
incident sur l'une ne doit pas contraindre l'autre.

## Déploiement côté serveur Tune

L'état actuel est volontairement inoffensif :

1. `PLUGIN_PUBLIC_KEY` est vide → la vérification est court-circuitée et
   `plugin_signature_required` ne peut pas être activé, quoi qu'en dise le
   réglage. Un test (`the_plugin_key_is_still_pending_marketplace_signing`)
   épingle cet état et **échouera dès qu'une clé sera embarquée** — c'est le
   rappel de passer à l'étape suivante.
2. Une fois le marketplace signant et la clé embarquée : la vérification tourne
   pour de vrai, mais un échec n'est que journalisé (`warn!`) tant que
   `plugin_signature_required` vaut `false`. Cette période sert à repérer les
   artefacts non signés restants sans casser d'installation.
3. Quand les logs sont propres : basculer le défaut de
   `plugin_signature_required` à `true`. Un artefact non signé ou mal signé est
   alors refusé en `403`, à l'installation **comme à la mise à jour**.

L'étape 3 est un changement de comportement pour les utilisateurs qui auraient
installé des plugins tiers hors marketplace — à annoncer dans le CHANGELOG.
