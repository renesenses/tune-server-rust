# SDK premium : première tranche de #4363

Statut : **expérimental**, contrats source et outillage. Aucun traitement du
serveur n'est déplacé ou supprimé par cette tranche. Les quatre plugins premium
installables restent à réaliser et qualifier.

Base de lecture : serveur `af70d7e251735d8be2c5d7ddc9d6539f61e2ac34`
(`rc/v0.9.153`), analyse initiale sur `73707a08`, web `e7787947`.
L'inventaire et les empreintes des fichiers de référence sont conservés dans
`premium-sdk-matrix.json`. Les empreintes servent à identifier une référence,
pas à prouver sa fidélité audio.

## Décisions

Le SDK expérimental vit dans `sdk/`, workspace Cargo indépendant avec son
lockfile et sa CI. Les projets générés n'importent ni `tune-core` ni
`tune-server`. Les adaptateurs vers ces deux caisses appartiendront à l'hôte.

Deux modèles sont nécessaires : processeur PCM par flux pour EQ/crossfeed,
outil batch utilisant des services hôtes pour convertisseur/Dé-ploc. Spectre et
VU restent des services transverses disponibles sans plugin premium.

Le premier mode de composition est **source Rust**, afin de qualifier les
contrats sans confondre migration et compatibilité binaire. La distribution
installable visée utilisera des paquets natifs officiels signés et une ABI C
versionnée ; elle n'est pas fournie par ces traits Rust. Aucune `Vec`, `String`,
référence Rust ou table de trait ne doit traverser cette future frontière.

Le runtime WASM du serveur existe ; le guide historique et certains commentaires
qui nient son existence sont obsolètes. Il est distinct de ce SDK et ne fournit
pas aujourd'hui ses services audio/jobs. Aucune capacité manquante n'est réputée
présente par la seule existence d'un manifeste.

## Livré dans cette tranche

| Élément | Preuve visée |
|---|---|
| Types audio et validation des blocs | Format, canaux, trames et limites validés ; PCM entier conservé sans conversion implicite. |
| Négociation SDK/capacités | Refus d'une capacité requise absente, de versions incompatibles, de doublons et de distribution native non implémentée. |
| Observation | Axes/résolution/provenance cohérents, filtre zone/génération et file bornée qui perd l'ancien. |
| Contrats batch | Sources, lecture par blocs, seek, codecs, écrivains temporaires, métadonnées, annulation et artefacts. |
| Testkit | Capture DSP hors périphérique et hôte batch mémoire ; résultats explicitement distincts de codecs et sorties réels. |
| Scaffolding | Deux projets indépendants, compilation et exécution de leurs tests ; refus de l'écrasement et des versions inconnues. |
| Référence API | Rustdoc sur les types réellement compilés, avec un exemple exécuté comme doctest. |

Les exemples sont un gain et une copie PCM. Ils démontrent l'utilisation des
interfaces ; ce ne sont pas une réimplémentation de l'égaliseur/crossfeed ni du
convertisseur/Dé-ploc. Les messages UI sont typés mais aucun panneau n'est monté.

## Référence fonctionnelle à conserver

EQ : macro/assistant, graphique 10/15/31, paramétrique, filtres par canal,
pré-gain, presets, AutoEq, courbes, diagnostics, application à chaud et état
réel dans le chemin du signal. Préserver `zone_{id}_eq_profile` et les anciens
presets pendant la migration.

Crossfeed : même algorithme et mêmes paramètres, historique du retard entre
blocs, stéréo seulement, PURE/DoP, application à chaud et explication de la
disponibilité. **Source et chemin comptent tous les deux** : le pré-transcodage
streaming peut passer par `StreamingDsp`, contrairement au transcodage fichier
de bibliothèque. Ne pas réduire cette distinction à « local ou réseau » ou à
un booléen « fichier ». Le ticket #2742 suit déjà un écart entre disponibilité
annoncée et chemin streaming ; l'extraction ne doit pas figer cet écart comme
vérité du nouveau SDK.

Dé-ploc : seuil, rognage tête/queue, recherche du passage par zéro sur canal 0,
traitement du silence complet, FLAC/WAV, tags et ZIP. Le défaut serveur -60 dBFS
et la valeur -40 dBFS envoyée par l'écran sont deux cas distincts. Le nom de la
fonction ne promet pas une réparation de tous les clics d'une piste.

Convertisseur : sources piste/album/dossier, formats réellement disponibles,
qualité/cadence/profondeur, métadonnées/pochettes, résultats partiels, annulation,
destination autorisée et ZIP. Garder les vocabulaires historiques :
`state/converted/error` ET `status/completed/errors` pour le convertisseur ;
`status/completed/errors[{path,error}]` pour Dé-ploc.

## Branchement hôte restant

1. Résoudre l'applicabilité avant la sélection passthrough/décodage, puis préparer
   avec le format effectivement produit. Tester chaque bras local, radio,
   streaming, fichier, navigateur, OAAT et préchargement/gapless.
2. Préserver l'ordre du DSP, le headroom et les protections PURE/DSD/DoP.
   Formaliser le transfert d'état pour les paramètres à chaud et les changements
   de format ; publier `AppliedLive` seulement après application effective.
3. Adapter le tap existant et l'événement `playback.audio_levels`. Le point de
   mesure doit être réel ; un décodage indépendant de la source ne mesure pas
   le signal après DSP ni la sortie physique du renderer.
4. Implémenter les services batch avec codecs/DB/fichiers réels, autorisations,
   quotas, publication atomique, destinations et nettoyage. Le testkit mémoire
   ne remplace pas ces protections et ne valide aucun encodeur.
5. Connecter premium, migrations, anciens endpoints et clients. Définir le sort
   d'une lecture/un job en cours lors de la révocation et de la désinstallation.
6. Extraire les quatre algorithmes avec leurs tests et comparer à la référence
   avant toute réécriture ; conserver les défauts connus dans un registre séparé.
7. Livrer ABI/chargeur/paquets, signatures et rollback, puis SDK UI et panneaux.

Les zones de code réservées par #2219, #2742, #4073 et les autres lots audio
ne sont pas modifiées ici. Cette tranche peut être relue indépendamment ; leur
coordination est nécessaire avant le branchement de production.

## Porte de stabilité

Chaque exigence de la matrice garde séparément son contrat SDK et son état de
validation **production**. La vérification Python contrôle l'inventaire et la
référence des témoins ; elle ne lance pas de lecture audio et ne certifie pas
les propriétés mentionnées dans la matrice.

Le SDK ne devient v1 qu'après les quatre plugins externes complets, les 16
combinaisons d'installation, les anciens clients, le spectre sans plugin,
installation/migration/rollback et les chemins audio/formats applicables.
Les cas impossibles sont annoncés comme tels, les cas non vérifiés restent
ouverts. La CI Shrek/Linux et la cross-compilation ne remplacent pas les
exécutions CoreAudio/WASAPI/ALSA et les sorties réseau matérielles.

Exiger des contre-épreuves : retirer l'appel du traitement, supprimer le
spectre, ignorer une capacité requise ou perdre l'état entre blocs doit faire
échouer le témoin comportemental concerné. Ne pas modifier le test pour obtenir
un rouge. Les preuves finales sont consignées dans `premium-sdk-evidence.md`.
