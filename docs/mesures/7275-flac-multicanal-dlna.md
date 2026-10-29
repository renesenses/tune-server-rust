# Six canaux FLAC vers un lecteur DLNA

Demande : https://github.com/renesenses/tune-server-rust/issues/7275.

## Constat et cause

Le rapport automatique du fil 2494 a été ajouté à l'issue par le mainteneur :
https://github.com/renesenses/tune-server-rust/issues/7275#issuecomment-6464533784

La copie remise contient les réglages, la découverte du DMP-A6 et les journaux
jusqu'à la préparation de la piste suivante. Le site coupe la fin du rapport
et le journal borne le Sink : 392 entrées annoncées, 183 non affichées. Ces
limites ne permettent pas de certifier toutes les capacités de l'appareil.
Les lignes suivantes suffisent cependant à identifier la réduction côté Tune :

- À 21:56:56, `dlna_sink_sonde` indique `canaux_max=Some(2)`. Les entrées
  visibles comportent `audio/L16;rate=44100;channels=1`, puis `channels=2`,
  `audio/flac:*` et `audio/x-flac:*` sans précision de canaux. Le profil AAC
  multicanal également présent ne prouve rien pour le FLAC.
- À 21:59:08, `dlna_canaux_annonces_sondes` retient deux canaux, puis
  `reduction_de_canaux_pour_le_renderer` indique source 6, cible 2,
  `canaux_declares=None`, origine `renderer`.
- À 21:59:11, `transcode_to_temp_file_stages` indique un FLAC à 96 kHz,
  24 bits et **2 canaux**. L'envoi de l'URI au lecteur intervient ensuite.
  Le même plafond est réutilisé depuis le cache pour la piste suivante.

Sur la base `3036a3ec782e858272feaea18fc9523b5fbad566`,
`tune-core/src/outputs/dlna.rs:4046` prend le maximum des `channels=` **tous
formats confondus**. `tune-core/src/orchestrator/resolve_direct.rs:154` reprend
ce résumé comme limite du lecteur et le mémorise seulement par appareil.
`tune-core/src/orchestrator/resolve_local.rs:1498` l'applique à la piste FLAC.
La limite LPCM devient donc, à tort, une limite FLAC.

Le réglage de sortie par défaut n'est pas la cause de cette lecture : les
canaux viennent de la piste (`resolve_local.rs`, `relever_la_source_et_la_zone`)
et le journal attribue explicitement le plafond au renderer, sans disposition
choisie. Le décodage reçoit déjà la cible deux canaux dans
`orchestrator/transcodage.rs:509` (`decode_to_pcm_routee`). Il applique alors
le mélange demandé. Le DIDL et le `protocolInfo` sont envoyés après ce
transcodage : ils n'ont pas provoqué la perte des quatre canaux.

## Correction

- Lire la limite dans les seules entrées HTTP du MIME servi, y compris son
  alias `audio/x-…`. Les paramètres de profil et les autres codecs ne sont pas
  des preuves pour ce MIME.
- Mémoriser cette limite par appareil **et format** ; journaliser le MIME
  avec la valeur sondée ou relue du cache.
- Décider des canaux après le choix du traitement, notamment lorsqu'il fait
  passer le flux en WAV.
- Reconnaître le FLAC même quand son annonce porte `;channels=6` : une annonce
  paramétrée ne doit pas faire basculer la négociation en WAV.
- Conserver le repli existant lorsqu'une limite applicable ou une disposition
  choisie l'impose. Le nombre de canaux réellement servi alimente déjà le DIDL
  et le chemin du signal ; ce dernier affiche la réduction et son origine
  (`tune-server/src/routes/zones/signal_path.rs:1871`).

Sans limite FLAC annoncée, Tune conserve les six canaux. Cela ne constitue pas
une promesse que le DMP-A6 les restituera : sa capacité FLAC multicanale réelle
reste à confirmer sur le matériel. Le correctif retire une réduction indue
côté serveur ; il ne fabrique aucune capacité du lecteur. Pour un appareil
réellement stéréo dont l’annonce FLAC ne précise aucune limite, la disposition
stéréo choisie dans la zone reste un plafond explicite. La seule annonce LPCM
ne suffit plus à imposer ce plafond au FLAC.

## Essais ciblés, sans son

Le banc `tune-stream-http/src/multichannel_7275_tests.rs` génère un FLAC 96 kHz,
24 bits, six voies distinctes. Il appelle la lecture réelle, avec une sortie
`DlnaOutput`, un ConnectionManager/AVTransport simulé et le vrai routeur de
flux HTTP. Le lecteur simulé télécharge l'URI reçue, puis les octets sont
décodés sans sortie audio. Toutes les connexions restent sur la boucle locale.

Trois cas : FLAC six canaux annoncé ; FLAC sans limite avec LPCM stéréo
(extrait pertinent du rapport) ; FLAC stéréo malgré un autre format à huit
canaux. Chaque cas réchauffe d'abord le cache avec le LPCM. Le banc compare
les canaux décodés, le DIDL et les informations du flux ; les cas six canaux
exigent en plus l'identité du fichier et des six voies. Le cas stéréo vérifie
le repli et le libellé explicite associé aux canaux servis.

La lecture réelle sur un Eversolo, le client web et les autres formats audio
ne sont pas éprouvés sur du matériel dans cette proposition.

## Contre-épreuves

Les tests sont restés inchangés. Chaque retrait de code a été précédé d'une
copie du fichier corrigé, puis annulé en recopiant cette sauvegarde.

1. Dans `resolve_direct.rs`, remplacer le calcul par MIME par
   `let canaux = caps.canaux_max;`. Commande :
   `cargo test -p tune-stream-http --lib multichannel_7275_unknown_flac_does_not_inherit_lpcm_limit`.
   Le test compile et échoue : `DIDL incohérent`, avec
   `nrAudioChannels="2"` au lieu des six attendus. Bilan : 0 réussi, 1 échec.
2. Dans `protocol_sink_supports_mime`, rétablir la comparaison du champ MIME
   entier, sans retirer ses paramètres. Commande :
   `cargo test -p tune-stream-http --lib multichannel_7275_receives_six_channel_flac_unchanged`.
   Le test compile et échoue : `le format servi reste le FLAC`, alors que le
   DIDL reçu annonce `audio/wav`, 16 bits. Bilan : 0 réussi, 1 échec.

Après restauration, `cargo test -p tune-stream-http --lib multichannel_7275`
repasse au vert : 3 réussis, 0 échec. Les tests de lecture des annonces passent
également : `cargo test -p tune-core --no-default-features --lib multichannel_7275`
(2 réussis) et `cargo test -p tune-core --no-default-features --lib canaux_annonces_4573`
(6 réussis). Aucun test de la suite complète ni audit de sécurité n'a été lancé
localement ; ces contrôles appartiennent à la CI.

## Alignement des témoins existants après la CI

La première CI a relevé deux bancs anciens qui attribuaient encore au FLAC la
limite LPCM du relevé Denon. Le relevé `SINK_DENON` reste inchangé. Une annonce
**synthétique** `SINK_FLAC_STEREO` protège désormais le repli explicite, et un
nouveau témoin exige que le relevé Denon laisse passer les six et huit canaux
FLAC. Le test de journal exige aussi le MIME associé à la valeur en cache.

`cargo test -p tune-core --no-default-features --test canaux_reseau_branchement_4573 --test canaux_origine_journal_6599`
passe : 6 tests de branchement et 1 test de journal réussis. Le banc HTTP
utilise le client partagé du projet, conformément à la garde relevée par la
CI. Après cette adaptation,
`cargo test -p tune-core -p tune-stream-http --no-default-features --lib multichannel_7275`
passe : 2 tests d'annonces et 3 tests de réception réussis.
