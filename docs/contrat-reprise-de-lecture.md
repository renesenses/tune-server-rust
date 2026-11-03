# Contrat « Reprise de lecture » — Fabien / #7280

28/10/2026 — Bertrand Clech / OpenAI Codex / bertrand-20261028-reprise-r8m2.
Écrit avant les modifications de code. Une copie versionnée dans chaque dépôt accompagnera sa PR.

## Décisions

Seuls titre, album, playlist et podcast figurent dans ce widget. Un contexte n'est enregistré qu'au démarrage effectif d'un titre, jamais au simple ajout à la file. « Lire », « Aléatoire », « Lire ensuite » et « Ajouter à la file » portent le contexte ; une piste lancée depuis un album/une playlist conserve cette origine.
Bertrand : 1 « Aléatoire » → conserver l'ordre aléatoire mémorisé ; 2 et 3 « OUI » → suivre aussi les titres seuls et appliquer les mêmes règles à Qobuz/TIDAL.

## Lecture

`GET /api/v1/home/continue-listening?limit=N&zone_id=Z` conserve une liste JSON et ses champs historiques. Les entrées nouvelles portent `resume_key` (opaque), `last_track_title`, `position_ms` (podcast et titre seul), `context_type`, `context_id`, `source`, `title`, `cover_path`.
Album/playlist : la vignette est celle du conteneur ; le libellé visible est le dernier titre démarré (`last_track_title`). Podcast : vignette et nom du podcast (`title`), épisode identifié séparément.

`POST /api/v1/home/continue-listening/{zone_id}/resume` avec `{"resume_key":"…"}` reconstruit la file mémorisée de ce contexte, sélectionne le dernier titre démarré et le rejoue depuis zéro pour un album ou une playlist ; la suite reste dans la file. L'ordre aléatoire mémorisé est conservé, y compris après redémarrage. Un identifiant de service n'est jamais interprété comme un identifiant de bibliothèque. Le serveur résout de nouveau les pistes par leur service, sans mémoriser un flux temporaire.
Titre seul : vignette du titre, reprise à sa position mémorisée, sans recul (décision de Bertrand du 28/10 à 20 h).
`POST /podcasts/play/{zone_id}` accepte en plus `podcast_id` (flux RSS ou identifiant stable) ; les clients envoient l’image du podcast, avant celle propre à l’épisode. Une seule entrée par podcast identifié, mise à jour avec son dernier épisode démarré. Ancien client : repli sur le nom transmis.

Podcast : relancer le même épisode à `max(0, position_ms - X * 1000)`. Le réglage serveur `continue_listening_rewind_seconds`, exposé par `GET/PATCH /system/config`, est un entier de 0 à 300 ; défaut : **10 secondes**, confirmé par la décision de coordination du 28/10 (section « SESSION REPRISE (n° 4) — DÉCISION 28/10 sur le recul des podcasts »). La position est persistée pendant l'écoute et lors d'une pause/arrêt.
Un objet absent ou périmé rend une erreur ; aucun repli silencieux vers un album ou une piste locale d'identifiant homonyme.

## Compatibilité

Les clients utilisent la reprise serveur quand `resume_key` existe ; les anciennes entrées gardent leur chemin historique avec `start_index: position` quand disponible. Un ancien serveur reste lisible. Les nouvelles entrées ne proposent jamais artiste, label ni radio dans ce widget ; leurs autres usages restent inchangés.

## Preuves requises

Serveur : contexte démarré puis GET ; file reconstruite et dernier titre rejoué (album/playlist/titre seul, local et service) ; ordre aléatoire ; contexte ajouté mais jamais démarré absent ; contexte démarré depuis un ajout présent ; podcast persisté et recul configuré/borné à zéro. Tests sans sortie sonore. Contre-épreuve compilable en retirant le correctif, restauration par copie puis vert.
Clients : décodage, libellé/vignette, requête sur la zone active, fallback ancien serveur et erreur ; réglage web. Builds/tests Shrek uniquement sauf simulateur iOS muet sur Mac.

### Ordre aléatoire matérialisé par un client
Un `POST /zones/{zone_id}/play` peut porter `context_shuffle: true` lorsque le client a mélangé lui-même la liste. La tête puis les ajouts conservent cet ordre ; le serveur ne le retire pas au titre suivant. Ce champ facultatif ne change pas le drapeau d’aléatoire de la zone. Une reprise normale reconstruit le conteneur complet même si plusieurs titres seulement avaient été sélectionnés. Une reprise mélangée conserve son instantané sans consulter un nouvel ordre du conteneur.
