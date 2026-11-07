Tune 1.0.0-rc10, dixième version candidate de Tune 1.0.

## Nouveautés

- **Ma collection physique** : un nouveau greffon compare votre collection Discogs à votre bibliothèque et aux services de streaming, dans une grille de pochettes gardées en cache sur le serveur. Une entrée dédiée apparaît dans la barre latérale, juste après Tune Circle.
- **Mon thème** : choisissez toutes les couleurs de l'interface, par profil. Un greffon peut aussi fournir un thème, et vous pouvez envoyer le vôtre à un contact Tune Circle, qui l'applique d'un clic.
- **Crosstalk** : le greffon d'atténuation de la diaphonie entre enceintes rejoint le catalogue des extensions, avec son écran de réglage ; sur une même zone, il désactive le crossfeed et l'indique.
- **Dossiers** : lisez ou ajoutez à la file un dossier avec tous ses sous-dossiers, dans l'ordre ; le filtre Répertoire de la Bibliothèque descend dans les sous-dossiers et propose de revenir à toute la bibliothèque.
- **Radios** : une barre « Plus de filtres » dans Mes radios et l'Annuaire permet de chercher par critères ; quatre nouveaux genres arrivent dans l'annuaire.
- **Radio en lecture** : les flux d'une station sont rangés par qualité ; Lecture en cours montre le logo de la station et un bouton « Mes radios » ; le pays d'écoute se règle dans Réglages › Général.
- **Labels** : les logos des labels s'affichent, partagés par mozaiklabs.fr ; l'état du serveur suit l'avancement des pochettes d'albums et des logos de labels.
- **Volume** : le curseur est gradué en dB, les limites de confort et de sécurité apparaissent en couleur, et une limite de confort au-dessus de la limite de sécurité est refusée.
- **Oxygen et métadonnées** : modifiez un champ sur tout un résultat filtré ; excluez des dossiers de l'enrichissement automatique des métadonnées.
- **Tune Circle** : zoom sur une zone du globe de la vue Monde ; les playlists Tune Circle ont leur pastille dans les filtres de Playlists ; une playlist de service se partage depuis sa fiche.
- **Widget de bureau** : la fenêtre mini reprend la vue mini de l'interface web et gagne une recherche d'artistes, Qobuz et TIDAL, avec vignettes et navigation au clavier.
- **Sortie AES67** : une sortie expérimentale vers les réseaux audio Dante et Ravenna compatibles AES67, désactivée par défaut, se trouve dans Réglages › Expérimental.

## Améliorations

- **Ce navigateur** : la piste suivante est préparée avant la fin pour un enchaînement plus fluide, et la zone « Ce navigateur » du serveur se joue sans réglage préalable.
- **Reprendre l'écoute** : la carte nomme l'album, la playlist ou l'épisode repris, et une écoute terminée quitte la liste.
- **Historique et titres phares** : les titres d'un album ou d'une playlist ouverts vont du plus récent au plus ancien ; les titres phares d'un artiste ajoutent ceux que vous écoutez le plus, toutes sources confondues.
- **Bibliothèque** : les filtres par DR et par année d'origine sont nettement plus rapides sur les grandes bibliothèques ; les colonnes de la liste des pistes se redimensionnent et chaque écran retient leur largeur ; l'analyse acoustique explique sa cadence et montre son avancement.
- **Égaliseur** : saisie libre de la fréquence et du Q, validée à la fin de la saisie ; la suppression des préréglages importés demande confirmation ; la correction de pièce et la calibration du DAC appliquent le profil d'égaliseur complet.
- **Playlists** : l'import .m3u, .m3u8 et .dpl se trouve dans le Gestionnaire de playlists et annonce le vrai nombre de titres ; Importer, Transférer et Comparer disent pourquoi ils échouent.
- **Pochettes** : la jaquette intégrée est lue dans toutes les balises, face avant d'abord ; la recherche en ligne suit la relecture du disque ; l'illustration rectangulaire d'une playlist Qobuz s'affiche entière.
- **Messages** : les derniers messages d'erreur restés en français sont traduits dans la langue de l'interface ; une enceinte AirPlay 2 qui refuse la connexion signale un appairage manquant ; un identifiant d'application refusé par Qobuz n'est plus présenté comme un mot de passe incorrect.
- **Licence** : l'écran indique que le Premium vient de votre compte Mozaiklabs, sans réclamer de clé.
- **Accueil** : « Autres versions » ne bloque plus en attendant les services de streaming.
- **ASIO** : quand la prise en charge du DSD ne peut pas être vérifiée, Tune le signale et explique la capacité de la sortie.

## Corrections

- **Lecture** : basculer entre PURE et égaliseur ne relance plus une zone arrêtée ; décocher « Lire en boucle par défaut » remet bien la zone sans boucle ; les transitions entre pistes compatibles restent continues.
- **OpenHome** : la pause et l'arrêt retirent la piste suivante préparée du lecteur, et la position de fin est conservée pendant le passage à la suivante.
- **Radios** : avec un pays d'écoute inconnu, le flux choisi n'est plus remplacé par un flux à 96 kbit/s ; une station ajoutée depuis la console garde le lien avec sa fiche ; un échec isolé ne suspend plus les métadonnées des radios ; « Dernières lues » affiche la bonne date ; la playlist Qobuz créée depuis les favoris radio apparaît aussitôt.
- **Bibliothèque et profils** : les filtres des favoris et des notes suivent le profil actif ; « Les plus écoutés » ne compte plus une écoute plusieurs fois ; l'enrichissement ne remplace plus un genre existant ; les pistes UPnP gardent leur année et leurs genres ; un identifiant MusicBrainz déjà attribué n'est plus posé une seconde fois.
- **Collections et playlists** : une modification refusée est signalée, un dossier fautif ne vide plus la liste, et toutes les pages de playlists sont lues.
- **Greffons et compte** : désinstaller un greffon natif actif le désactive durablement ; la première liaison au compte Mozaiklabs se rattache au profil unique même sans courriel.
- **Divers** : l'import des playlists au scan dit pourquoi il n'importe pas ; un échec de lecture des réglages est signalé au lieu de revenir aux valeurs par défaut ; une mise en veille de l'ordinateur n'est plus signalée comme un blocage ; l'ancienne interface ne survit plus à une mise à jour du serveur ; la barre à VU-mètres ne déborde plus entre 1280 et 1800 px ; le mode WASAPI reste visible sur le réglage par défaut de Windows ; l'audio n'est plus compressé à l'envoi.
- **Sécurité** : correctifs de durcissement.
