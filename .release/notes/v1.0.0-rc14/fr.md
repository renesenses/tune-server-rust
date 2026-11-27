Tune 1.0.0-rc14 reprend tout le contenu des pré-releases rc13.90 et rc13.91, et décrit ici ce qui change depuis la rc13.91. Merci aux testeurs : beaucoup de ces points viennent de vos retours.

## Nouveautés

- **TESS, préréglages Enregistrer et Recharger** : dès qu'un réglage s'écarte du préréglage choisi, TESS affiche « modifié ». **Enregistrer** écrase le préréglage d'origine, **Recharger** y revient. Un préréglage livré avec Tune, comme « Pure », ne s'écrase pas : « Enregistrer sous… » en fait une copie.
- **TESS, préréglage DSD appliqué tout de suite** : un préréglage qui change la cible DSD (par exemple DSD128 → DSD256), chargé en mode « appliquer immédiatement », relance la piste à sa position, au lieu d'attendre la piste suivante. Le suréchantillonnage est une fonction Premium.
- **Marge (headroom)** : en Automatique, ou à la main de −12 à 0 dB par pas de 0,5 dB. Le panneau donne la valeur réellement appliquée, ou dit pourquoi elle ne l'est pas.
- **Marge du suréchantillonnage vers le DSD** : la marge manuelle n'est plus appliquée deux fois (−12 dB donnait −24 dB, c'est maintenant −15 dB, dont −3 dB pour le modulateur DSD, affichés sur leur propre ligne).
- **Mesure de la pièce guidée vers 75 dB(C)** : avec un micro calibré, l'écran affiche le niveau mesuré, son écart à 75 dB(C) et une consigne de volume. La règle est prudente, la conversion n'ayant pas encore été vérifiée au calibrateur : Tune ne conseille jamais de dépasser 80 dB(C) estimés et coupe le signal à 90 dB SPL. La correction mesurée au micro est Premium ; celle calculée sans micro reste gratuite.
- **Sécurité auditive** : décocher un égaliseur, passer en PURE ou en Bypass all, ou retirer une convolution, un crossfeed ou un repli mono ne fait plus remonter le niveau d'un coup : il rejoint celui d'avant par un fondu d'environ une demi-seconde. Si vous avez monté le volume entre-temps, le voyant « niveau réduit » s'allume et « Rendre le niveau » libère le surplus. Cela vaut aussi sur les sorties réseau (UPnP, OAAT).
- **Chemin du signal fidèle** :
  - sur une sortie réseau, il suit l'ordre réel du flux, jusqu'au volume du lecteur ;
  - l'étape d'un filtre de réponse impulsionnelle s'appelle « Convolution » ;
  - le filtre garde le nom du fichier .wav ou .zip importé ;
  - après un changement de cible DSD, il affiche la cadence DSD réellement jouée, et annonce la suivante ;
  - la fenêtre passe au vert : l'orange est réservé aux vraies alertes.
- **Convolution** : le panneau affiche la courbe du filtre chargé, canal par canal, et la phase à la demande.
- **Bypass all** : l'icône TESS de la barre de lecture est atténuée quand TESS est contourné, et la page TESS affiche « TESS contourné ».
- **Page TESS harmonisée** : accent vert TESS, zone en cours rappelée près du badge, panneau Headroom grisé sur une sortie réseau, et mesure « Sans micro (estimation) » ou « Avec micro (recommandé) ».
- **Sortie réseau** : la carte Suréchantillonnage dit que le suréchantillonnage ne s'y applique pas.
- **Interface harmonisée** : polices embarquées, aussi hors ligne ; Bibliothèque, Live Radio, Serveurs multimédia et Concerts suivent la nouvelle charte.

## Correctifs

- **Radio vers un Sonos** (et tout lecteur réseau plafonné) : une radio en 192 kHz est servie à la cadence que le lecteur accepte. Avant, le lecteur s'arrêtait aussitôt, en boucle.
- **768 kHz** : la liste « Fréquence max » d'une zone propose 768 kHz et 1 536 kHz.
- **Bibliothèque** : la grille des albums ne se fige plus pendant le défilement d'une grande collection.
- **Lecture dans l'onglet du navigateur** : le déplacement dans la piste déplace vraiment la lecture, même depuis un autre appareil.
- **Aide** : le bandeau du support prioritaire ne s'affiche plus pour un compte Premium.
- **Licence** : la date d'expiration est lue correctement, et Réglages › Licence dit pourquoi le palier est gratuit.
- **Convolution** : un filtre qui amplifie certaines fréquences ne sature plus.
- **Podcasts longs** : un épisode dont l'hébergeur coupe régulièrement la connexion n'est plus interrompu.
- **Paquet Debian dans un conteneur LXC** (Proxmox) : le service démarre même sans espaces de montage privés.
- **Premier lancement** : l'assistant ne dit plus « activés » des services sans compte, et il enregistre le dossier de musique choisi. « Mes radios » vide ouvre l'annuaire.

## Sécurité et comptes

- **Session renouvelée** : avec « Rester connecté » (décoché par défaut), la session du navigateur se renouvelle tant qu'elle sert, jusqu'à 90 jours. Sinon, 24 h comme avant. Le menu de l'avatar propose « Se déconnecter de ce serveur ».
- **Récupérer l'accès administrateur** : lancez `tune-server --reinitialiser-admin` (ou `--reset-admin`) **sur la machine du serveur**. Le code à usage unique obtenu, valable 15 minutes, se saisit dans « Mot de passe oublié ? » depuis cette machine ; sans navigateur, ajoutez `--mot-de-passe-stdin`. Les sessions ouvertes sont fermées. Le guide d'installation (section 9) donne la commande pour chaque système.
- **Profil `admin`** : le profil administrateur créé d'office s'appelle désormais `admin` ; un nom choisi est gardé, et `default` reste accepté.
- **Mot de passe administrateur dès la première configuration** : sur une installation neuve, l'assistant le fait créer.
- **Serveur sans authentification** : l'administration est acceptée depuis le réseau local, plus seulement depuis la machine du serveur. **Mise en garde** : sans authentification, les appareils du réseau local peuvent administrer le serveur ; activez-la si votre réseau n'est pas de confiance. Le réglage « Administration depuis cette machine seulement » rétablit l'ancienne règle.

## Télécommandes

- Sur un serveur protégé, les apps pas encore à jour redemandent le mot de passe toutes les 24 h, comme avant.

## À vérifier chez vous

- **« Bypass all » dans un préréglage** : un préréglage qui change à la fois Bypass all et la cible DSD, en mode « appliquer immédiatement », relance la piste à sa position. Vérifié seulement au banc : dites-nous si la reprise est propre.
