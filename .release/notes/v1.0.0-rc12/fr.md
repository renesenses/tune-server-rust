Tune 1.0.0-rc12, douzième version candidate de Tune 1.0. Plusieurs écrans arrivent dans une nouvelle présentation dessinée par Levente, designer de la communauté Tune.

## Nouveautés

- **Nouvelle interface** : Tune Circle redessiné (Découvrir, Messages, Playlists partagées, Mon cercle) ; fiche d'album avec description, crédits, détails et un onglet Versions avec le numéro de catalogue ; page artiste avec grand portrait, chiffres et onglets ; Réglages dans une colonne plus lisible ; État du serveur et tableau de bord des écoutes, avec choix de la période.
- **Accueil** : un nouveau widget « Aperçu des écoutes (nouvelle présentation) », à ajouter dans Accueil › Modifier, avec un panneau Zones ; vignettes plus grandes avec année et genre.
- **Bibliothèque** : épinglez les filtres de votre choix avec le bouton Filtres ; musique classique avec Chef, Ensemble / orchestre et Œuvre ; nombre de canaux et filtre Stéréo ; carte et filtre par pays de l'artiste ; « Remplacer le chemin » d'un dossier de musique ; détection des faux Hi-Res, désactivée par défaut.
- **Fiche technique d'une piste** : spectrogramme et forme d'onde par canal, aussi pour les pistes Qobuz et TIDAL de la bibliothèque.
- **Radio de fin de file** : quand la file se termine, Tune continue avec des titres similaires, marqués « Radio » ; réglable zone par zone.
- **Arrêt programmé** : la musique s'arrête à l'heure choisie ; sur Tune OS, la machine peut aussi s'éteindre.
- **Playlists** : nombre d'écoutes et dernière écoute sur chaque ligne ; croix Retirer et « Supprimer de cette playlist » ; retrait d'un titre de votre propre playlist Qobuz.
- **Radio** : filtres Sans perte, Haute qualité et Hi-Res sur une ligne ; une station de l'annuaire se modifie pour vous ; une radio s'ajoute à une playlist.
- **Lecture** : DSD natif sur la sortie locale Linux (ALSA) ; fréquence PCM fixe par zone ; mode exclusif réglé zone par zone ; zones supprimées restaurables ; vraie vérification AccurateRip des CD.
- **DLNA** : flux continu entre les pistes pour les renderers qui raccordent mal deux titres, désactivé par défaut ; le volume maître d'un groupe se règle depuis n'importe quel membre.
- **Streaming** : choix du pays du catalogue TIDAL ; nouveautés Qobuz identiques à celles du lecteur Qobuz ; Spotify avec un assistant de connexion, et Spotify Connect joue dans la zone choisie.
- **Chemin du signal** : un clic sur un traitement ouvre ses réglages ; la lecture groupée montre chaque sortie.
- **Réglages et interface** : recherche par réglage, taille du texte, barre de progression en forme d'onde, une seule zone de recherche sur tous les écrans, saisie vocale dans l'assistant.

## Améliorations

- **Vitesse** : collections intelligentes et sauts alphabétiques beaucoup plus rapides sur une grande bibliothèque ; les grandes listes ne ralentissent plus le serveur.
- **Mémoire** : Tune rend la mémoire libérée pendant une longue écoute ; la mémoire du transcodage est bornée.
- **Démarrage du son** : YouTube démarre plus vite ; TIDAL n'attend plus le téléchargement complet de la piste sur une sortie locale ou OAAT.
- **Statistiques** : un seul total de titres et d'albums sur tous les écrans, une seule entrée par codec, recherche dans tout le classement.
- **Tune Circle** : emojis dans les Messages ; quand l'écoute chez un contact est fermée, Tune cherche le même titre dans vos sources.
- **Applications mobiles** : CarPlay joue directement sur le téléphone, avec Collections et un onglet Radio ; Android gagne le catalogue TIDAL, l'historique des radios et des podcasts, et un widget Radios favorites.
- **Windows** : un bandeau prévient quand le pare-feu filtre Tune ; pastille WASAPI ou ASIO sur les zones locales.
- **Docker** : l'assistant de partage SMB fonctionne dans l'image ; le lecteur CD est utilisable dans le conteneur.
- **Traductions** : pluriels justes dans les 15 langues.
- **Dépendances** à jour, dont SQLite 3.54 embarqué.

## Corrections

- **Interface web** : plus d'écran noir après une mise à jour de l'interface.
- **Sourdine** : une zone en sourdine reste muette quand une nouvelle piste démarre ; un réveil ou une limite de volume ne lève plus la sourdine.
- **Lecture** : un fichier abîmé ne fige plus la zone, Tune passe à la suite ; un épisode de podcast terminé ne se rouvre plus à la fin.
- **AirPlay 2** : la lecture reprend là où elle s'était arrêtée après une pause, et démarre au volume de la zone, jamais à pleine puissance.
- **DLNA et groupes** : la lecture reprend après une erreur 701 ; la piste suivante est préparée d'avance ; un appareil qui ne répond pas ne bloque plus les autres zones ; Sonos prend le nom de son modèle.
- **HQPlayer, Devialet et Diretta** : position et état suivis, reconnexion automatique ; l'ampli Devialet répond de nouveau ; plus de faux curseur de volume sur Diretta.
- **OAAT** : DSD natif affiché et bit-perfect ; DSD1024 reconnu, avec repli en PCM sur un récepteur limité.
- **Radios** : plus de stations en double ; une radio refusée dans votre pays passe à son flux suivant.
- **Spotify, Qobuz et TIDAL** : playlists et favoris Spotify suivent la nouvelle API ; un refus Qobuz n'est plus présenté comme des identifiants erronés ; le préchargement TIDAL ne retarde plus la lecture.
- **Bibliothèque** : les écoutes sont comptées par titre ; un trait d'union ne crée plus de faux artiste ; les disques d'un coffret sont mieux reconnus ; un titre sans année prend celle de son album.
- **Windows** : un DAC absent n'envoie plus le son sur les haut-parleurs du PC ; la surveillance des dossiers couvre les partages réseau.
- **PostgreSQL 18** : les favoris UPnP fonctionnent de nouveau.

## Sécurité

- Administrer Tune depuis une autre machine du réseau demande désormais de s'identifier (Tune OS : jusqu'à la nouvelle image).
- Les webhooks ne peuvent plus viser la machine Tune elle-même.
- La configuration de Home Assistant et de Hue demande les droits d'administration ou la machine Tune elle-même ; un Home Assistant installé sur la même machine que Tune n'est plus pris en charge.

## Problèmes connus

- **DSD en DoP sur la sortie locale** : il peut jouer à pleine échelle après du PCM atténué, sans tenir compte du volume. Gardez le volume de l'amplificateur bas quand vous passez d'un titre PCM à un titre DSD en DoP (prochaine version).
- **Audit d'intégrité** : de nombreux FLAC peuvent être signalés à tort comme corrompus ; ne supprimez aucun fichier sur ce seul verdict.
- **Windows ASIO piloté par un autre serveur** : plus de son après Pause puis Lecture ; passez au titre suivant.
- **Après la mise à jour** : si l'interface ne change pas, forcez le rechargement (Cmd+Maj+R ou Ctrl+Maj+R). Le premier démarrage est plus long, le temps de créer deux index.
