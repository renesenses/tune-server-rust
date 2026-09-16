# CPace et Sendspin : provenance des temoins #3326

JP Robbe / OpenAI Codex / jp-robbe-20260916-3326-pairing.

- draft-vectors.json : sections G_25519 et X25519_points de
  draft-irtf-cfrg-cpace-21, auteurs Michel Abdalla, Bjoern Haase et Julia Hesse.
  https://datatracker.ietf.org/doc/html/draft-irtf-cfrg-cpace-21#appendix-B.1
  Extraction du fichier distribue avec cpace 0.1.0 (Apache-2.0).
- generator-vectors.json : un vecteur du brouillon et 128 calculs de reference
  deja compares dans le banc Shrek cpace-map-check.
- reference-vectors.json : 64 echanges completes par cpace 0.1.0 ; variations
  de longueurs autour des seuils de l'encodage LV, scalaires reproductibles de
  fixture uniquement, confirmations des deux roles et ISK.
- flow-vectors.json : six cas (statique, chiffres dynamiques, QR ; chaque AEAD),
  derives avec cpace 0.1.0 et aiosendspin
  b6f8564d07b212d77bfb026b80baa23435d9e591. Les champs chiffres proviennent de
  cryptography (AESGCM/ChaCha20Poly1305), pas de Tune.

Les nombres fixes et les scalaires de ces fichiers sont des donnees publiques
de test, jamais des identifiants d'un appareil ni des secrets de production.
La construction publique de Tune tire toujours un scalaire du CSPRNG.

Regeneration des 64 echanges et six cas Sendspin :
le script tests/sendspin/generer_vecteurs_pake.py prend un dossier de sortie.
Il emploie seulement les bibliotheques tierces epinglees, sans charger Tune.
Le SID est assemble selon la specification epinglee, car le parcours
aiosendspin b6f8564 omet encore le numero de tour. Les API CPace et les helpers
de code sont donc la reference comparee ; son SDK complet n'est pas valide ici.
La licence distribuee avec cpace est conservee dans LICENSE-cpace.txt.
