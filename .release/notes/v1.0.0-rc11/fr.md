Tune 1.0.0-rc11, onzième version candidate de Tune 1.0.

## Nouveautés

- **Correction de pièce** : un greffon, désactivé par défaut, mesure la pièce au micro (balayage, réponse impulsionnelle, RT60) ou simule ses modes de basse sans micro, puis propose une cible et un égaliseur automatique qui ne fait que couper, avec un profil par zone ; sans Premium, la convolution par réponse impulsionnelle s'éteint en fondu.
- **Contrôle parental** : règles par profil dans Réglages › Profils, masquage des contenus explicites, plages horaires et budget d'écoute.
- **Suréchantillonnage** : un greffon Premium de suréchantillonnage haute qualité se branche dans la sortie locale, avec son réglage par zone.
- **Requête avancée** : interrogez la bibliothèque par une expression, la même qui définit les collections intelligentes.
- **Une fiche par album** : les versions d'un album sont regroupées et se choisissent dans la fiche ; de nouveaux filtres Qualité DSD et Multicanal et le nombre de canaux apparaissent.
- **Notes et statistiques** : notes personnelles en Markdown et statistiques de la bibliothèque par répartition.
- **Catalogue et spectrogramme** : exportez la bibliothèque en catalogue HTML imprimable, avec les filtres en cours ; affichez le spectrogramme d'une piste depuis son menu ; un bouton arrête la gravure du DR entre deux pistes.
- **Cartes du monde** : carte des pays et filtre par pays dans la Bibliothèque ; le globe trace les frontières, les côtes et les grands fleuves, et nomme le pays au survol.
- **Podcasts** : ajoutez un podcast par l'adresse de son flux RSS.
- **Mixes TIDAL** : la rangée « Vos mixes » de Streaming › TIDAL affiche et lit les mixes personnels du compte.
- **Import d'égaliseur** : importez un fichier d'égaliseur (AutoEq, Equalizer APO, OPRA) avec un aperçu des bandes avant de l'appliquer.
- **Biographies** : liens vers les artistes et albums de Tune et vers la source ; biographies et notes d'album complétées à la demande depuis Qobuz, sans rien écraser et de façon réversible.
- **Playlists** : remplacez un titre de service indisponible à sa place et réparez la playlist.
- **Tune Circle** : écrivez à tout un cercle dans Messages ; les cercles généraux et le consentement du fil passent par le relais.
- **Collection physique** : pochette de chaque disque, recherche sur les services de streaming, parcours par dossiers, genres et styles.

## Améliorations

- **Radios** : filtre par qualité de flux minimale dans « Plus de filtres », « Mes radios » séparé de « Dernières écoutées », flux par défaut toujours jouable dans le pays d'écoute.
- **Tune tested** : Tune propose d'ajouter un appareil après une heure d'écoute sans erreur, depuis un bouton dédié ; un appareil déjà au catalogue ajoute un foyer.
- **Bibliothèque** : réglage « Bibliothèque élargie aux favoris des services », activé par défaut ; recherche par début de mot plus rapide ; la recherche de la grille exige tous les mots ; période De / À sur la frise des années ; défilement des titres plus fluide.
- **Listes de pistes** : l'ordre des colonnes se change et chaque écran le retient ; l'écran d'une playlist gagne pochettes, taille des vignettes et en-tête compact ; pastilles de source dans le Gestionnaire de playlists.
- **Accueil et reprise** : choisissez les panneaux de l'aperçu des écoutes ; « Reprendre l'écoute » affiche un sous-titre par nature, reprend un podcast avec titre, pochette et position, et sans doublons.
- **Historique** : une écoute n'est validée qu'au bout de 30 s ou de la moitié du titre ; « Lecture » après Stop garde l'album ou la playlist en cours ; les épisodes de podcast et les radios portent le nom de la station.
- **Statistiques d'écoute** : les écoutes importées (Roon, Plex, m3u) comptent dans les plus écoutés ; les listes intelligentes et l'Historique comptent comme la Bibliothèque ; une écoute Qobuz ne compte plus pour l'album local du même nom.
- **Crosstalk et égaliseur** : courbe séparation / coloration et réponse aux enceintes dans l'écran de réglage, séparation visée dès 1 dB ; graphe paramétrique figé par défaut (Modifier / Terminé) et couleur par canal.
- **Extensions** : un panneau « État des greffons » pour l'administrateur ; une fiche du catalogue et son greffon local ne font plus qu'une extension.
- **Notifications** : les erreurs persistent, se mettent en pause au survol et se regroupent ; les derniers messages d'erreur en dur sont traduits ; le texte brut de l'exception est replié dans « Détails ».
- **Analyses de fond** : l'avancement s'affiche, la priorité est basse et le défaut prudent après le scan ; l'enrichissement reprend où il en était et retente après un 503 ; « Lancer la passe » des logos de labels agit même en pause.
- **Support et accès distant** : le rapport de bogue joint des journaux ; le lien, le QR code et le bouton copier reviennent dans Réglages › Accès distant.
- **Lecture** : les motifs de transcodage sont nommés et le FLAC est servi tel quel quand rien ne s'applique ; Suivant et Précédent sautent les titres refusés par le service.

## Corrections

- **AirPlay 2** : le bouton « Pair » jumelle par le composant AirPlay 2 ou nomme sa cause ; l'attente du code PIN ne bloque plus la lecture et fermer sa fenêtre l'annule ; la session se rouvre après une pause ; la position avance et une pause ne boucle plus ; le composant existe pour Raspberry Pi 64 bits.
- **DLNA et OpenHome** : enchaînement sans blanc des pistes DSD, « suivant » plus rapide, pause faite sur l'appareil respectée, sortie de la machine plus en double, volume OpenHome sans attente, liste chargée par Kazoo conservée, TIDAL CD plus rapide au Play.
- **Volume** : une sortie locale neuve ne part plus à pleine échelle ; la sourdine ne laisse plus passer un volume relu ; chaque position du curseur donne un niveau distinct ; le volume WASAPI et ASIO s'entend tout de suite ; Tune suit l'appareil sous le plafond de sécurité.
- **Windows** : le raccourci s'ouvre sans fenêtre de console ; un DoP que la sortie ne porte pas part en PCM ; la découverte réseau passe le pare-feu ; une température illisible ne bride plus les analyses ; la mise à jour lance son script de bascule au moment de sortir.
- **Mises à jour et licence** : sous Unix, le nouveau binaire et l'interface web sont validés avant de remplacer l'ancien, et un échec est explicite ; retirer la licence libère aussi la machine auprès du site.
- **Bibliothèque** : les photos et vidéos d'un serveur UPnP n'entrent plus ; un coffret FLAC + CUE forme un seul album ; les éditions DSD et CD restent deux albums ; une compilation faite à la main n'est plus éclatée ; une playlist supprimée n'est plus recréée au scan ; une relecture sans balise d'artiste ne fait plus passer l'album sous « Unknown Artist » ; ajouter un dossier ne scanne que ce dossier.
- **Pochettes et analyses** : en dernier recours, la plus grande image du dossier ; analyse Faux Hi-Res sur toute la durée du morceau ; MP3 sans blanc ; l'audit d'intégrité ne déclare plus tronqués des FLAC intacts.
- **Services de streaming** : la recherche Spotify rend des résultats, un refus n'est plus muet et les favoris dépassent 50 titres ; TIDAL › Favoris ne gèle plus Tune ; un compte Qobuz sans favoris rend vite sa liste vide ; une piste convertie démarre dans Safari.
- **Réseau et stockage** : la découverte reprend quand le réseau arrive après Tune ; un montage SMB qui échoue dit pourquoi, y compris sous Docker ; la sauvegarde aboutit sur PostgreSQL ; l'extraction de CD refuse une destination pleine ou non inscriptible ; Tune laisse les ports Squeezebox à un serveur Lyrion de la même machine.
- **Écrans** : l'écran Concerts s'ouvre sans licence ; « Dégrouper tout » réapparaît ; Lecture en cours suit le changement d'album ; une session de streaming expirée garde son onglet « Reconnecter » ; Nouveau réveil reste visible.
- **Raspberry Pi 5** : le ventilateur de l'Active Cooler se remet en marche (nouvelle image Tune OS).
- **Sécurité** : correctifs de durcissement.
