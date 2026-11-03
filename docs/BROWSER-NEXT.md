# Préchargement de la zone navigateur (#5975)

Ce contrat est le prérequis **serveur** du gapless navigateur autorisé pour
rc9. Il ne rend pas à lui seul le client web gapless : le lecteur doit
précharger, décoder et programmer les transitions, puis être qualifié dans
Safari. Les correctifs Safari #6669 et le diagnostic #7015 restent inchangés.

## Préparer pendant la piste courante

`POST /api/v1/zones/{id}/browser-next`

```json
{"current_stream_id":"identifiant-du-flux-courant"}
```

Réponse 200 :

```json
{"next":{"stream_url":"http://serveur/stream/identifiant.wav","stream_id":"identifiant","mime_type":"audio/wav","duration_ms":1000,"title":"Titre"}}
```

La file, le titre affiché et le flux courant ne changent pas. La décision
est celle de la fin naturelle : répétition, ordre aléatoire matérialisé et
bannis du profil de session. Une préparation répétée de la même ligne
retourne le même flux. `next: null` signifie fin de file ou radio en direct.
La résolution peut échouer : garder alors la lecture courante et le repli
habituel du client. La route ne lance aucun appareil de sortie.

Le client applique à `stream_url` le même traitement d'origine que pour le
flux courant (accès local ou distant). Il ne remplace pas encore ce dernier.
Une sonde de plage ou des octets préchargés ne changent ni la piste courante
ni l'historique.

## Adopter quand le flux suivant joue réellement

`POST /api/v1/zones/{id}/browser-next/playing`

```json
{"current_stream_id":"ancien-flux","next_stream_id":"flux-prepare","position_ms":100}
```

Le client envoie la position positive mesurée par son lecteur après le début
réel du flux préparé. Le serveur adopte **ce même flux**, avance les
métadonnées et l'historique sans rouvrir le fichier. Réponse 200 :
`{"status":"adopted"}`. Relire ensuite la zone pour synchroniser l'écran.
Ne pas appeler également `next` pour cette transition.

Une répétition de cette confirmation reçoit 409 et n'avance pas deux fois.
Un changement de lecture, une autre ligne de file, un autre flux ou une
position nulle invalident l'adoption. Sur 409, abandonner le préchargement et
relire la zone ; ne pas forcer l'ancien flux. Les anciennes routes et les
clients qui ne préchargent pas gardent leur comportement.

Les deux routes sont réservées aux zones `browser` (409 sinon, 404 pour une
zone absente). L'adoption partage le verrou de décision de `next` pour
éviter deux déplacements simultanés. La préparation ne bloque pas le suivant
manuel pendant la résolution : elle revérifie ensuite la lecture et la file,
et abandonne son flux si elles ont changé.

## Limites de la preuve

Les tests HTTP utilisent des fichiers PCM muets temporaires, sans sortie
physique. Ils prouvent la disponibilité préalable de l'URL, la conservation
de la piste courante et l'adoption sans réouverture. Ils ne prouvent pas une
transition audible sans blanc dans Safari. Les échecs Tourist/tttroys restent
sans cause établie : les fils 2178/2246 ne fournissent pas leur format ni un
journal couvrant ces échecs. Aucun essai de service réel n'est revendiqué.
