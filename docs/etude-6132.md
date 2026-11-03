# Étude #6132 — deux sorties locales sous Windows, et Audio / Zones / Appareils

Lecture seule, 13/10/2026. Serveur relevé sur `batch/bugs-rc5-20261012`
(`178859289`), client web sur `main` (`ac244add`). Aucun code de production
n'est changé par ce document. Les décisions de produit sont listées à la fin,
elles ne sont pas tranchées ici.

Le retour (fil 2215, rc3, Windows, plusieurs sorties locales : un DAC USB en
WASAPI et en ASIO, une interface USB, un ampli AV) pose deux questions :

1. peut-on envoyer **la même écoute sur deux sorties** ?
2. les écrans « Audio », « Zones », « Appareils » et le menu « Zones » sont-ils
   redondants ?

## 1. La même écoute sur deux sorties locales : aujourd'hui, non

### Ce qu'un groupe fait vraiment

- Le geste « Lecture groupée » écrit une liste dans le réglage JSON
  `zone_groups` (`POST /zones/group`, `tune-server/src/routes/zones/groupes.rs`,
  `create_group`). La validation (`validate_group`) refuse moins de deux
  zones, une zone inconnue, et deux zones sur le même `output_device_id`
  (`group_same_output`, #1702). **Aucune restriction par type de sortie** :
  deux zones locales sur deux cartes différentes passent.
- **Aucun code ne propage la lecture d'une zone du groupe aux autres.**
  `zone_groups` n'est lu que par les routes de groupe, `sync_status`
  (affichage), l'instantané WebSocket, la fusion de zones doublons et
  l'import/export de configuration. Rien dans `tune-core/src/orchestrator*`,
  `tune-core/src/playback`, ni les sorties locales.
- Le volume de groupe est appliqué zone par zone (`group_volume`) ;
  `calibrate_group` répond 501 ; `sync_delay_ms` est enregistré sans
  consommateur dans la chaîne audio.
- Le contrat publié le dit : `generic_group_synchronization_contract()`
  (`tune-server/src/routes/zone_manager.rs`) déclare `supported: false`,
  `transport: "independent_renderers"`, `alternative: "oaat"` (#2215).
- `tune-core/src/zones/group.rs` (`GroupManager`, meneur et suiveurs) n'a
  aucun appelant hors de ses tests. `zones.group_id` n'est écrit que par
  l'import de configuration.
- Une seconde famille de routes, `/zone-manager/groups`, écrit le même réglage
  sans validation ni contrôle Premium ; le client ne l'appelle pas.

Le seul multiroom synchronisé est OAAT (`/zone-manager/oaat-groups`,
`OaatMultiroomOutput`) : ses membres sont des points de diffusion réseau
`(hôte, port)`, pas des zones. Une sortie locale n'y entre pas.

### Les contraintes des sorties locales

- **ASIO : un seul flux dans tout le processus.** `ASIO_DEVICE_LOCK`
  (`tune-core/src/outputs/asio_exclusive.rs`) est un verrou global, pris pour
  toute la session, pas un verrou par carte. Deux zones ASIO ne jouent jamais
  en même temps, même sur deux cartes. `canaux_de_carte_6171.rs` l'écrit :
  deux zones sur la même carte se relaient, les jouer ensemble « demanderait
  un mélangeur commun ».
- Le backend (`local_audio_backend`) et le mode exclusif WASAPI sont des
  réglages **de la machine**, corrigés périphérique par périphérique selon
  l'hôte qui a énuméré le nom (`openable_local_backend`, #1770/#3230/#5353).
- Une zone a **un** `output_device_id`, donc **une** `LocalOutput`. Il n'existe
  ni sortie agrégée, ni duplication d'un flux vers deux périphériques
  (le seul « fan-out » est `tune-core/src/audio/tap.rs`, pour l'analyse).
- Deux cartes ont deux horloges : deux flux ouverts séparément dérivent l'un
  par rapport à l'autre. Rien dans le code ne le traite pour les sorties
  locales (seul OAAT porte un horodatage absolu).

### Côté client

- Le seul geste de groupement est la section « Lecture groupée » de
  `ZoneConfigModal.svelte` (`api.groupZones` → `POST /zones/group`), qui
  propose toutes les autres zones sans filtre.
- On n'y arrive que par un **clic droit** sur la pastille de zone de la barre
  de lecture (`TransportBar.svelte`), monté avec `groups={[]}` et
  `onGroupChanged={() => {}}` : un groupe existant n'y est ni affiché, ni
  coché, ni dégroupable, et « Dégrouper tout » n'apparaît jamais.
- `docs/getting-started/fr.md` (« Multi-room ») renvoie à « Réglages → Groupes
  de zones → Créer un groupe », chemin qui n'existe plus dans le client v2.

### Ce qu'on peut répondre au testeur, en l'état

« Ce n'est pas possible aujourd'hui avec deux sorties locales. Le groupement
de zones existe, mais il ne fait que piloter le volume ensemble : il ne
recopie pas la lecture d'une zone sur l'autre. En ASIO, une seule zone joue à
la fois sur la machine. » La réponse elle-même reste à `tune-parole`, après
avis de Bertrand.

### Pistes, sans arbitrage

| Piste | Ce que ça demande | Limite |
|---|---|---|
| A. Dire vrai dans l'interface | masquer ou désactiver « Lecture groupée » tant qu'elle ne propage rien, ou l'étiqueter « volume seulement » ; corriger `getting-started` | ne donne pas la fonction |
| B. Propager la lecture d'un groupe | à la lecture sur le meneur, lancer le même élément sur chaque suiveur (deux flux indépendants) | dérive des horloges, départs décalés ; ASIO exclu (verrou global) ; pas bit-perfect garanti en mode partagé |
| C. Sortie locale double | une sortie qui ouvre deux périphériques et leur écrit le même PCM, avec rééchantillonnage d'appoint pour l'horloge | chantier audio ; WASAPI seulement ; ASIO hors d'atteinte sans revoir le verrou |
| D. Passer par OAAT | un point de diffusion Tune par sortie locale, puis un groupe OAAT | lourd pour une seule machine ; pas de chemin aujourd'hui |

## 2. Audio / Zones / Appareils : ce que chaque écran porte

| Écran | Où | Contenu |
|---|---|---|
| Menu « Zones » | `Sidebar.svelte` → `ZonesV2.svelte` (« Destinations sonores ») | créer une zone (locale ou découverte), renommer, supprimer, volume ; « Changer la sortie » (Expert) ; fusionner un doublon, rattacher un orphelin, AirPlay, RTT ; paires stéréo, OAAT, délais multiroom (Expert) |
| Réglages ▸ Audio | `SettingsV2.svelte`, sections `localAudio`, `netDevices`, `ignoredDevices`… | backend et mode WASAPI **globaux** ; liste des sorties locales avec visibilité ; « Créer une zone sur cet ordinateur » ; appareils réseau, ignorés ; Squeezebox, Sendspin, HQPlayer, DSD, DSP réseau, égaliseur Expert, diagnostic |
| Réglages ▸ Zones | `v2Settings.ts` | deux interrupteurs : « Créer automatiquement les zones », « Me suivre » ; aucune liste de zones |
| Réglages ▸ Appareils | sections `devices`, `perZone`, `sauvegardeReglages` | un compteur et deux liens (« Ouvrir les Zones », « Aller à Audio ») ; **une carte par zone** : DSD, fréquence max, volume fixe, mono, bit-perfect, canaux, gain, correction de pièce, éditeur d'appareil |
| Clic droit sur la zone | `ZoneConfigModal.svelte` | renommer, « Lecture groupée », audio de la zone (gapless, DSD, volume fixe, fondu), canaux ASIO, mono, correction de pièce, éditeur d'appareil |

### Où ça se recoupe

- **Les réglages d'une zone vivent à deux endroits** : Réglages ▸ Appareils ▸
  « Réglages par zone » et la fenêtre du clic droit. Seule la correction de
  pièce est un composant partagé ; le reste a deux balisages, qui peuvent
  diverger.
- **Le même mot pour deux contenus** : l'onglet Réglages ▸ « Zones » (deux
  interrupteurs) et la page « Zones » du menu (toutes les zones). « Zones de
  lecture » désigne aussi bien l'interrupteur de création automatique que le
  titre des délais multiroom.
- **L'onglet « Appareils » ne montre pas d'appareils** : il renvoie à Audio
  pour la visibilité, et porte les réglages par zone.
- **Le choix de la sortie est séparé de ses réglages** : la sortie d'une zone
  se change dans la page Zones, le backend et le mode exclusif dans Audio, le
  reste dans Appareils.
- Les mêmes listes (`getAudioDevices`, `getDevices`) alimentent la création
  de zone (page Zones) et la visibilité (Audio) : deux écrans, une donnée.

### Un regroupement possible, sans arbitrage

- Tout ce qui touche **une zone** au même endroit : la page Zones du menu, la
  carte d'une zone ouvrant ses réglages (sortie, audio, canaux, correction),
  et la fenêtre du clic droit réduite à un raccourci vers cette carte.
- Tout ce qui touche **la machine** dans Audio : backend, mode exclusif,
  visibilité des sorties et des appareils, services réseau.
- L'onglet « Appareils » disparaît ou ne garde que la sauvegarde ; l'onglet
  Réglages ▸ « Zones » rejoint la page Zones (ses deux interrupteurs).

Ce regroupement touche le client web (renesenses/tune-web-client) seulement ;
le serveur ne change pas.

## Décisions laissées à Bertrand

1. Le groupement de zones : le retirer, le dire « volume seulement » (piste A),
   ou en faire une vraie lecture groupée (B, C ou D) ?
2. Si lecture groupée : accepte-t-on deux flux non synchronisés entre deux
   cartes locales (B), ou faut-il une sortie double à horloge corrigée (C) ?
3. ASIO : reste-t-il « une zone à la fois sur la machine » ?
4. Les écrans : accepte-t-on le regroupement « une zone / la machine »
   ci-dessus, et la disparition de l'onglet « Appareils » ?
5. La réponse au testeur du fil 2215.

Refs #6132
