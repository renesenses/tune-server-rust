Tune 1.0.0-rc13, treizième version candidate de Tune 1.0. Merci à Gros Bidon, r5yk5, Tades, cpadiou77, Pierre, Daniel LEVY, Lio, sashasha, ajm, Jean Valjean, Semakawa, Jon Rotaetxe, hifi25nl, aqs, Christopher J, FabienM, Begastor et Régis pour leurs signalements et leurs essais, à alec_eiffel pour le Smart Upsampler, à cometdom pour Qobuz et à Levente pour le design.

## Nouveautés

- **Suréchantillonneur** : le Smart Upsampler de Rémi (alec_eiffel) joue au même niveau qu'en son absence, et un changement de réglage s'entend tout de suite.
- **Lecture et sorties** : Sendspin et Devialet AIR reçoivent le son traité (égaliseur, greffons, suréchantillonneur). L'arrêt programmé peut éteindre l'ordinateur ; contrôles multimédia du bureau sous Linux.
- **Crossfeed** : gratuit, avec ses réglages et ses préréglages.
- **Convolution** : un fichier de réponse impulsionnelle par fréquence d'échantillonnage.
- **DSD** : le DSD d'un serveur UPnP se joue en natif sur les lecteurs réseau exigeants et deux pistes DSD s'enchaînent sans blanc.
- **Bibliothèque** : vos balises personnelles deviennent des tags ; sélection de plusieurs albums pour une action groupée ; filtre par instrument.
- **Source altérée** remplace « Hi-Res douteux », pour tous les formats, avec la coupure mesurée.
- **Forme d'onde** plus fine, aussi pour les pistes CUE, Qobuz et TIDAL (sur option).
- **File d'attente** : les albums se replient et se déplacent d'un geste.
- **Accueil** : 5 playlists « Parce que vous aimez… » et les widgets Top artistes, Écoutes fréquentes, Écouter plus tard et Reprendre l'écoute.
- **Services** : page « Pour vous » de TIDAL ; 200 nouveautés Qobuz par genre.
- **Tune Circle** : les membres d'un cercle se voient s'ils l'acceptent, et la vue Monde montre tous les cercles.
- **Interface web** : turc, authentification activable depuis l'interface et historique par titre ou par écoute.
- **Tune Client (app de bureau)** : trouve seul les serveurs du réseau et pilote le serveur local.
- **Windows** : démarrage de Tune avec Windows, proposé par l'installeur.
- **App iOS** : CarPlay en 5G plus complet, barre latérale de retour sur iPad.

## Améliorations

- **Enchaînements** : ReplayGain « Auto » garde le gain d'album entre deux pistes du même album ; AAC, Opus et pistes CUE s'enchaînent sans blanc.
- **Lecture et sorties** : même courbe de volume en AirPlay 1 et 2 ; une radio démarre plus vite sur un lecteur réseau.
- **Windows** : en WASAPI exclusif, repli vers l'autre famille de fréquences au lieu d'arrêter la piste.
- **Devialet AIR** : volume par pas de 1 dB, jusqu'à 192 kHz.
- **DSD** : la détection du DSD natif est plus prudente et se réévalue.
- **Grandes bibliothèques** : Répertoires s'ouvre sans attendre, les tâches de fond ne bloquent plus le reste.
- **Services** : client de connexion Google propre à Tune pour YouTube ; Spotify et TIDAL plus robustes en cours de lecture.
- **Interface web** : égaliseur paramétrique plus précis et confirmation avant de supprimer.
- **App iOS** : l'écran verrouillé pilote la zone même à l'arrêt ; sans DAC, le curseur règle le volume d'iOS.
- **Mise à jour et diagnostic** : la mise à jour remplace aussi les programmes livrés avec Tune ; les rapports de bogue masquent les noms.

## Corrections

- **Lecture et sorties** : la reprise fonctionne après une pause qui avait rendu le DAC ; « Tout répéter » ne confond plus un arrêt passager avec la fin d'une piste ; un DAC allumé après Tune est trouvé sous Windows.
- **Zones** : un appareil déjà lié nomme sa zone ; en OAAT, une reprise rouvre la session au lieu de rester muette.
- **DLNA, UPnP et OpenHome** : plus de Stop parasite sur Musicbook Source, sourdine stable, Linn Klimax DS/2 reconnu.
- **AirPlay** : la sortie AirPlay 1 ne démarre plus au maximum après un redémarrage.
- **Chemin du signal** : il n'annonce plus de traitement qui n'a pas lieu.
- **Bibliothèque** : les éditions DSD et CD d'un même album ne sont plus fusionnées ; la gravure du DR ne gèle plus le serveur.
- **Services** : favoris Qobuz et TIDAL avec leur durée ; les rubriques Spotify ne sont plus en double.
- **Radios** : les stations FIP retrouvent leur logo.
- **Greffons et crossfeed** : un greffon installé agit sans redémarrage ; le crossfeed n'est plus verrouillé sur OAAT ni dans le navigateur.
- **Interface web** : la grille défile jusqu'au bout ; le bandeau « le serveur ne répond pas » disparaît dès qu'il répond.

## Sécurité

- Plusieurs durcissements de sécurité.
- Les journaux et les rapports de bogue masquent les identifiants que portent certaines adresses de flux.
- Remplacer le chemin (Réglages › dossiers de musique) vérifie désormais les dossiers et n'est plus accessible depuis un autre appareil sans authentification.

## Changements de comportement

- **Tune Circle** : avec l'authentification, réservé aux administrateurs ; en lecture seule depuis l'écoute à distance.
- **YouTube** : les comptes connectés avant cette version doivent être reconnectés une fois.
- **Radio de fin de file** : de nouveau désactivée par défaut ; recochez-la dans la zone si vous l'aviez choisie.
- **Égaliseur** : couper l'égaliseur garde son préampli jusqu'à « Rendre le niveau ».
- **Multiroom** : groupe gratuit jusqu'à 3 zones ; au-delà, Premium. Les paires stéréo restent gratuites.
- **CUE avec INDEX 02** : débuts de piste corrigés au prochain scan, historique conservé.
- **Tune OS** : le mot de passe temporaire s'affiche sur l'écran de la machine ou sur la carte de démarrage, plus sur la page de connexion ; l'accès à distance reste fermé tant qu'il n'est pas changé.
- **Profils et historique** : avec l'authentification, seul l'administrateur gère les autres profils.
- **Remplacer le chemin** : sans authentification, depuis la machine où tourne Tune.

## À vérifier chez vous

Ces changements ont passé les tests automatiques sans essai sur du vrai matériel. Dites-nous ce que vous constatez :

- **Sendspin et Devialet AIR** avec un traitement actif.
- **Windows** : mini-lecteur, WASAPI exclusif, démarrage avec Windows.
- **AirPlay 1** : niveau au démarrage et entre les morceaux.
- **DSD vers un lecteur réseau** (Atoll ST200, Eversolo).
- **Linn Klimax DS/2** en OpenHome, **Tune Client sur macOS** et **CarPlay en 5G**.

## Problèmes connus

- **DSD en DoP sur la sortie locale** : après un PCM atténué, il peut jouer à pleine échelle. Gardez le volume de l'amplificateur bas.
- **UPnP après un changement d'adresse** : une piste peut ne pas jouer ; réindexez ce serveur.
- **Audit d'intégrité** : quelques fichiers peuvent être signalés à tort ; ne supprimez rien sur ce seul verdict.
- **Windows ASIO piloté par un autre serveur** : plus de son après Pause puis Lecture ; passez au titre suivant.
- **Après la mise à jour** : si l'interface ne change pas, forcez le rechargement (Cmd+Maj+R ou Ctrl+Maj+R).
