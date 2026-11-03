Tune 1.0.0-rc9, neuvième version candidate de Tune 1.0.

## Nouveautés

- **Tune Circle** : messagerie privée entre contacts, avec cartes musique à écouter d'un clic, effacement de vos propres messages et signalement. Les messages ne sont pas chiffrés de bout en bout et peuvent être lus par Mozaiklabs après un signalement.
- **Tune Circle** : « Écrire » depuis la liste des contacts ouvre directement la conversation ; Entrée envoie le message ; affichage adapté au téléphone.
- **Tune Circle** : vous retrouvez les cercles auxquels vous êtes invité et pouvez y partager une playlist ; une pastille et une liste regroupent les notifications, réglables par type.
- **Tune Circle** : la vue Monde montre la communauté de façon anonyme, par pays et par zones d'environ 110 km. Vous pouvez retirer votre présence de la carte publique.
- **Radios** : la recherche générale trouve vos radios et celles de l'annuaire, avec plusieurs mots dans n'importe quel ordre. Une fiche présente la station, ses genres, son site et ses flux, et vous pouvez choisir votre flux préféré.
- **Cartes et podcasts** : explorez les radios par genre, pays et réseau, avec une vue liste ; parcourez les podcasts de plusieurs pays.
- **Reprise de lecture** : la reprise retrouve l'album, la playlist ou la file d'origine, y compris en lecture aléatoire. Pour les podcasts, elle recule de quelques secondes, réglable de 0 à 300 s.
- **Collections** : tri aléatoire dans les collections et les étiquettes, avec « Re-tirer » ; les albums favoris d'un service se classent par année et par qualité.
- **En lecture** : un bouton « Partager » copie le lien du morceau en cours.
- **Oxygen** : appliquez ou retirez un tag sur tout un résultat filtré, après aperçu et confirmation.
- **Apparence** : nouveau thème « Mozaiklabs » ; la page de connexion Mozaiklabs suit la langue et le thème de l'interface.
- **Langues** : le norvégien bokmål, le polonais et le grec arrivent dans Tune.
- **Android Auto** : Favoris, Playlists, Radios, File d'attente et Lecture en cours, avec les commandes de la zone choisie (pas encore essayé en voiture).

## Améliorations

- **En voiture** : avec Android Auto comme avec CarPlay, Tune Remote joue sur « Ce téléphone » ; la zone de la maison n'est pas touchée.
- **Radios** : les pochettes publiées par les stations s'affichent, avec le logo en repli ; l'annuaire se rafraîchit sans redémarrer Tune ; le choix du flux respecte les restrictions de pays.
- **TIDAL** : la recherche d'une version Hi-Res se poursuit quand la première réponse n'est qu'en qualité CD, dans les limites de votre abonnement.
- **HQPlayer** : les changements de piste sont plus fiables et une relance qui ne démarre pas est signalée.
- **Devialet** : avancer dans une piste Qobuz Hi-Res reprend sans couper le son ; pour Devialet AIR, le volume observé sur l'appareil s'affiche à part.
- **Volume** : le volume et la sourdine respectent les capacités de chaque sortie ; l'assistant IA ne monte le volume que par petits pas.
- **Sorties** : un appareil qui propose AirPlay et UPnP/DLNA apparaît sous ses deux formes ; ASIO et CoreAudio respectent mieux la grille de réaffectation des voies.
- **Bibliothèque** : liste compacte des appareils ; le filtre Répertoires s'applique aussi aux ajouts récents ; l'historique regroupe les écoutes d'une même radio.
- **Écoute à distance** : l'accès par compte se configure sans QR code, et les appareils associés au compte peuvent être retirés.

## Corrections

- **Lecture** : un ancien relevé de position ne termine plus le morceau après un déplacement ; les WAV très courts n'interrompent plus l'enchaînement ; la lecture reprend après un refus temporaire de changement de fréquence.
- **DLNA et zones** : les lecteurs DLNA reçoivent bien le FLAC multicanal annoncé ; changer l'appareil d'une zone en lecture arrête celui qu'elle quitte ; un transfert refusé laisse la lecture d'origine intacte.
- **TIDAL** : des erreurs de démarrage liées à certains liens audio sont corrigées.
- **Divers** : extraction de CD après Stop, réveils dans l'ordre, playlists créées affichées aussitôt, bandeau de mise à jour qui disparaît après l'installation, clés ListenBrainz et Last.fm avec PostgreSQL, sauvegardes sans caches HTTP.
- **Sécurité** : correctif du relais d'écoute à distance ; les détails seront publiés dans un avis séparé.

Spotify est livré sans preuve de lecture réelle : dites-nous sur le forum si la lecture fonctionne avec votre compte.
