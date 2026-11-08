# Réponses TIDAL des mixes (#7822)

Ces réponses suivent la forme de l'API TIDAL v1 telle que la bibliothèque
publique python-tidal la lit (`Session.mixes()` sur
`pages/my_collection_my_mixes`, `Mix.parse`, `pages/mix?mixId=`). Elles ont
été écrites à la main dans cette forme, avec des identifiants, titres et
images fictifs : elles ne viennent PAS d'un compte réel.

À la preuve sur un vrai compte, remplacer ces fichiers par les réponses
capturées (en retirant toute donnée personnelle) et relancer les témoins
`cargo test -p tune-core --lib -- mixes_7822`.

- `page_mes_mixes.json` : `/pages/my_collection_my_mixes`, trois mixes ;
- `page_mes_mixes_vide.json` : la même page pour un compte sans mix ;
- `page_accueil_avec_mixes.json` : `/pages/home`, une rangée mêlant mixes et
  albums (`MIXED_TYPES_LIST`) ;
- `page_mix.json` : `/pages/mix?mixId=…`, en-tête et deux pistes ;
- `mix_items.json` : `/mixes/{id}/items`, repli à enveloppes, dont une vidéo.
