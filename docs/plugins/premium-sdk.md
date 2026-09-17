# Extraction des quatre fonctionnalités premium — #4363

L'égaliseur, le crossfeed, le convertisseur et Dé-ploc disposent de projets SDK indépendants, de templates exécutables et d'une distribution native signée. Les façades Tune gardent les points d'insertion audio et les API existantes. Le spectre reste produit par l'hôte, même lorsque les quatre plugins sont désinstallés.

La [référence SDK](../../sdk/README.md) décrit les commandes, les types, les services hôtes, l'API d'installation et les limites. La [référence ABI](../../sdk/tune-plugin-abi/SAFETY.md) fixe les tailles, propriétaires, durées de vie, erreurs et règles de concurrence. Le Rustdoc est généré depuis les interfaces réellement compilées. La [matrice](premium-sdk-matrix.json) conserve les exigences et témoins, avec la référence historique `af70d7e251735d8be2c5d7ddc9d6539f61e2ac34` et ses empreintes.

## Répartition

| Hôte Tune | Plugin |
|---|---|
| Licence, installation, zones, sauvegarde des anciens profils/presets, autorisations | Validation de sa configuration et implémentation de son traitement |
| Pipeline, PURE/DoP, choix du format, lifecycle et ordre existants | EQ : cascades, pré-gain, historique, dither et diagnostics ; crossfeed : retard et mélange |
| Tap PCM, FFT, VU, génération, événements et abonnement borné | Consommation optionnelle des observations ; aucun calcul FFT obligatoire pour faire fonctionner le spectre |
| Résolution piste/album/dossier, codecs, rééchantillonnage, tags/pochettes, fichiers temporaires, ZIP | Politique de conversion et découpage Dé-ploc, progression, annulation et résultats par fichier |
| Requêtes authentifiées, navigation, thème/langue/zone, panneau existant | Client UI indépendant et panneau optionnel en iframe sandbox avec port privé |
| Vérification des paquets et activation au redémarrage | Bibliothèque de la cible exacte, manifeste et ressources signés |

## Compatibilité conservée

EQ : mêmes champs `EqProfile`, macro/environnement, graphique 10/15/31, paramétrique et canaux, types de filtre, pré-gain, presets, import AutoEq, état à chaud et compteurs. La réponse de fréquence du SDK vient des coefficients préparés, inclut le pré-gain et indique qu'il s'agit d'un aperçu. Elle n'est pas présentée comme une mesure du périphérique.

Crossfeed : même arithmétique, retard persistant, stéréo, mono intact, intensité et délai, garde PURE/DoP existante. La distinction streaming prétranscodé/fichier de bibliothèque reste dans l'hôte ; #2742 n'est pas redéfini par une règle simplifiée « local ou réseau ».

Convertisseur : mêmes résolutions de sources et encodeurs natifs/externes, qualité/cadence/profondeur, tags, pochette, ZIP et destination autorisée. Le vocabulaire historique `state/converted/error` et `status/completed/errors` est préservé. La publication utilise un lien atomique sans écrasement, y compris si une autre écriture gagne la course. Deux noms de sortie identiques produisent désormais une erreur par fichier au lieu de remplacer silencieusement le précédent.

Dé-ploc : même recherche de silence, rognage tête/queue, passage par zéro du canal 0, silence complet, FLAC/WAV. Défaut serveur -60 dBFS et -40 dBFS explicitement envoyé par le web restent distincts. La recherche historique de passage par zéro couvre la plage silencieuse ; elle n'est pas arbitrairement limitée à 50 ms. Ce plugin ne promet pas de réparer les clics au milieu d'une piste.

Spectre : événement `playback.audio_levels` conservé avec ajout de `play_seq`, `generation`, point/provenance et format. Le client écarte les anciennes pistes, anciens seeks, autres zones et tableaux incomplets. Le point actuellement offert est `decoded_source` ; demander post-DSP retourne une absence de capacité. Les axes/résolutions proviennent de l'analyseur réel, jamais de la taille FFT rembourrée seule.

## Livraison et migration

Les quatre implémentations sont réutilisées par les façades source et par leurs cdylibs. Cette transition préserve les installations existantes ; le code des références reste donc présent dans le binaire hôte. Un paquet natif installé remplace son fournisseur au prochain démarrage. Pas de compilation ou de clé privée sur le poste du client.

La migration marque sa fin après écriture des drapeaux manquants, conserve les choix explicites et ne réinstalle pas les plugins après désinstallation. Les seize combinaisons sont testées. Configuration et presets restent conservés lors d'un rollback ou d'une désinstallation.

Le serveur n'exécute jamais une bibliothèque installée avant vérification de sa signature et de son intégrité. Une erreur de chargement rend la fonctionnalité indisponible, visible dans le statut. Les instances en cours possèdent la bibliothèque jusqu'à leur destruction. Licence et installation sont vérifiées côté hôte pour les mutations et la préparation ; aucun accès réseau dans le traitement PCM.

Les tâches déjà lancées terminent ou sont annulées explicitement. Un journal persistant rend un travail perdu après redémarrage comme `interrupted`, sans reprendre automatiquement ni supprimer les fichiers publiés. Les statuts terminaux récupérés indiquent `download_available:false` : l'index ZIP en mémoire n'est pas restauré. Les destinations utilisateur restent intactes.

## Preuves et portes d'acceptation

La parité compare trois exécutions indépendantes : source historique extraite par `git show`, bibliothèque SDK et façades chargeant les vraies bibliothèques natives. Les tests de contrat vérifient les buffers, transitions, codecs hôtes, métadonnées, annulation, publication et erreurs. Les tests de paquets signent avec une clé publique de fixture non installée en production et vérifient succès puis refus d'altération.

Les preuves locales sont consignées dans [premium-sdk-evidence.md](premium-sdk-evidence.md). La CI SDK utilise trois OS. Les tests matériels CoreAudio/WASAPI, les cibles réseau, la charge multi-zone et l'écoute restent des portes de qualification de release : une compilation Linux ou une capture PCM ne vaut pas leur acceptation. Aucun merge, déploiement, publication ni changement des clés de confiance n'est inclus dans cette implémentation.
