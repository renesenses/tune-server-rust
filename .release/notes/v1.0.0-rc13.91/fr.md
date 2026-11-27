Tune 1.0.0-rc13.91 : **seconde pré-release de la rc14**, réservée aux testeurs. Ce n'est pas une version publiée : elle n'est pas proposée par la mise à jour automatique. Elle reprend tout le contenu de la rc13.90. Merci à celles et ceux qui la testent.

## Nouveautés

- **Égaliseurs combinables en série** : égaliseur graphique, paramétrique, casque et correction de pièce s'enchaînent dans la même chaîne TESS ; appliquer une correction de pièce n'écrase plus les autres égaliseurs. Les gains de l'égaliseur paramétrique sont élargis.
- **Bypass all complet** : un seul geste met toute la chaîne TESS hors circuit, et la rétablit à l'identique.
- **Greffon Casque** tiré de la base OPRA : le profil de votre casque est reconnu, avec préampli et niveau.
- **Greffon Speaker Setup** (réglage des enceintes) : distance, gain et phase, voie par voie.
- **Suréchantillonnage vers le DSD sur la sortie locale** : un modulateur sigma-delta dédié, et les capacités DSD de la sortie sont publiées.
- **Cible du suréchantillonnage au choix** : dans TESS, un écran permet de choisir vers quoi suréchantillonner, en PCM ou en DSD.
- **Test de niveau du micro** avant la mesure de la pièce.
- **Mesure à la fréquence native du micro**, de 44,1 à 192 kHz.
- **Réglages TESS à la volée** : désactiver une étape prend effet tout de suite, et le préampli reste borné.

## Améliorations

- **Correction de pièce** : la mesure de vérification entend la correction de la zone ; les écarts non corrigés sont expliqués, et la limite haute est réglable jusqu'à 1 kHz.
- **Correction de pièce** : la saisie, la dernière mesure et la correction appliquée sont retrouvées en revenant sur l'écran ; des consignes de placement du micro et un encadré « Avant la mesure » guident la mesure.
- **Mesure en mode PURE** : le volume est forcé à 100 % le temps du balayage, puis rendu tel qu'il était.
- **Chemin du signal** : une étape éteinte (suréchantillonnage, convolution) dit pourquoi, dans TESS et sur la page du greffon ; sous Windows en mode partagé, le suréchantillonnage vise la fréquence du mélangeur.
- **Crédits** : l'instrument plutôt que « Musicien », l'auteur distinct du compositeur, un nom par rôle.
- **Scan** : le code-barres présent dans les balises complète l'album s'il n'en a pas.
- **État du serveur** : un avertissement s'affiche si le cache des pochettes ne peut pas être créé.

## Corrections

- **Calibration** : le balayage ne s'arrête plus avant la longueur annoncée et ne reste plus bloqué dans la file ; une zone sans sortie est refusée clairement.
- **Calibration** : un pic au plafond ne bloque plus les bandes suivantes.
- **RT60** : « non mesurable (bruit) » plutôt qu'une valeur fausse ; les messages d'erreur restent lisibles au moins 10 s.
- **Avertissement de fréquence** : il n'apparaît plus que hors des fréquences acceptées.
- **Radio** : le changement de station n'est plus écrasé par l'ancienne station.
- **Pochettes** : sur une installation neuve, le cache est créé sous le dossier de données.
- **TESS** : un seul avis « piste suivante », et l'aide du bouton « Appliquer » est corrigée.

## Changements de comportement

- **Changement de chaîne TESS pendant la lecture** : une étape désactivée se coupe immédiatement ; une étape ajoutée ou réactivée s'entend à partir de la prochaine lecture.

## À vérifier chez vous

Ces points n'ont pas encore été testés sur du matériel réel :

- **Le suréchantillonnage vers le DSD sur un vrai DAC DSD** : stabilité, niveau, absence de clics.
- **La mesure de pièce à 44,1 kHz et à 96 kHz** sur une vraie interface audio, avec le test de niveau du micro.
- **Sous Windows, en WASAPI partagé** : lecture et suréchantillonnage.
- **Speaker Setup sur une sortie réseau** (UPnP, AirPlay ou OAAT) : distance, gain et phase.

Et aussi :

- **Plusieurs égaliseurs en série** avec Bypass all : le chemin du signal correspond-il à ce que vous entendez ?
- **Le greffon Casque** avec votre propre casque.
- **La mise à jour depuis la rc13.90.**

## Problèmes connus

- **SACD compressés en DST** : ces images ISO ne sont pas encore lues nativement.
- **VU-mètres** sur une piste MP3, AAC ou Ogg relayée : après un saut, ils se coupent jusqu'à la fin de la piste. Le son n'est pas touché.
- **Écoute à distance sur CarPlay** : la correction du relais est reportée à la rc15.
- **Serveur sans écran, authentification coupée** : pour changer de compte mozaiklabs.fr relié, activez l'authentification ou passez par un tunnel SSH vers localhost:8888.
