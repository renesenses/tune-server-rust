Tune 1.0.0-rc13.90 : **pré-release de la rc14**, réservée aux testeurs. Ce n'est pas une version publiée : elle n'est pas proposée par la mise à jour automatique. Merci à celles et ceux qui la testent.

## Nouveautés

- **TESS, la chaîne de traitement en 64 bits** : tout le traitement se fait en virgule flottante 64 bits de bout en bout, avec une seule quantification, à la toute fin.
- **Le même chemin pour toutes les sorties** : sortie locale, UPnP/DLNA, AirPlay, Devialet AIR et OAAT passent par TESS ; une sortie qui ne peut pas recevoir un traitement le dit clairement.
- **Égaliseur paramétrique** à bandes libres (fréquence, gain, Q), à côté de l'égaliseur graphique et de l'égaliseur casque (profils AutoEq).
- **Convolution** : filtres en .wav, en archive .zip ou en configurations Convolver (.cfg), un filtre par fréquence d'échantillonnage, changement sans coupure et latence annoncée.
- **Correction de pièce par la mesure** : un filtre FIR à phase mixte calculé sur plusieurs positions d'écoute.
- **Upsampling vers le DSD** (DSD64, DSD128, DSD256) par un modulateur sigma-delta.
- **Préréglages de chaîne** : enregistrez une chaîne complète et rappelez-la, tout de suite ou à la prochaine lecture.
- **Chemin du signal** : depuis la barre de lecture, étage par étage, ce qui est réellement appliqué, et pourquoi un étage ne l'est pas.
- **Greffons natifs** dans la chaîne : activation à chaud, format 64 bits, ordre et nombre de canaux respectés.
- **Interface en 20 langues** : arrivée du néerlandais, du chinois traditionnel, du russe et de l'ukrainien.
- **Authentification** : un bouton « Désactiver l'authentification » dans Réglages › Système › Authentification.

## Améliorations

- **ISO SACD** : le DSD natif est servi aux lecteurs réseau en DSF virtuel ; un DSD transporté en DoP s'annonce comme du DSD.
- **Écoute à distance** : en 5G, la première piste part en FLAC.
- **Appareils absents** : oubliés automatiquement après 30 jours (réglable), sans jamais supprimer de zone.
- **Cover+** : de meilleures pochettes, aussi pour les webradios (métadonnées ICY).
- **Lecture en groupe** : décalage réglable par zone ; « Précédent » sur la zone meneuse relance aussi les zones suiveuses.
- **Bibliothèque et streaming** : les passes de fond ne bloquent plus l'interface, le tri alphabétique des albums est conservé, artistes similaires depuis la fiche d'un service.
- **Messages de refus** plus courts, qui renvoient au bon réglage.

## Corrections

- **Les corrections des rc13.1 et rc13.2** sont incluses : connexion du compte administrateur, liaison du compte mozaiklabs.fr depuis le réseau local, pochettes avec l'authentification active, mise à jour sous Windows.
- **UPnP** : Tune ne s'indexe plus lui-même ; une piste indexée se joue à l'adresse actuelle de son serveur.
- **Lecture** : préchargement de 30 s sans tampon jeté ; un saut refusé par la source ne décale plus le flux.
- **Chromecast** se reconnecte ; la republication AirPlay ne coupe plus la lecture.
- **Podcasts** : les épisodes indisponibles sont signalés proprement.
- **Retours du forum** : historique, Qobuz, Spotify, bouton « Retour », recherche et iPad.

## Changements de comportement

- **Égaliseur paramétrique** : gratuit jusqu'à la rc13, il fait désormais partie de Tune Premium, comme la convolution, la correction de pièce, le declick en temps réel et les greffons tiers. L'égaliseur graphique, l'égaliseur casque et le crossfeed restent gratuits.

## À vérifier chez vous

- **Une chaîne TESS complète** (égaliseur, crossfeed, convolution ou correction de pièce) : le chemin du signal correspond-il à ce que vous entendez ?
- **L'import d'un jeu de convolution** et le changement de filtre pendant la lecture.
- **L'upsampling vers le DSD** sur un DAC compatible : stabilité, niveau, absence de clics.
- **Les préréglages de chaîne**, dans les deux modes d'application.
- **Une sortie réseau** (UPnP, AirPlay ou OAAT) avec un traitement actif.
- **La mise à jour depuis la rc13.x** et le bouton « Désactiver l'authentification ».

## Problèmes connus

- **SACD compressés en DST** : ces images ISO ne sont pas encore lues nativement.
- **VU-mètres** sur une piste MP3, AAC ou Ogg relayée : après un saut, ils se coupent jusqu'à la fin de la piste. Le son n'est pas touché.
- **Écoute à distance sur CarPlay** : la correction du relais est reportée à la rc15.
- **Serveur sans écran, authentification coupée** : pour changer de compte mozaiklabs.fr relié, activez l'authentification ou passez par un tunnel SSH vers localhost:8888.
