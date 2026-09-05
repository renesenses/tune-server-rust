# Carte de dÃ©coupe du cÅur

RelevÃ© sur `main` v0.9.133 (`9c7c6d80`), 4 septembre 2026. Anatomie mesurÃ©e
des quatre fichiers du cÅur, familles de l'orchestrateur avec leur couplage,
cibles du plan v1 (#2219) dans la sortie locale, et les gardes qui cassent Ã 
la premiÃ¨re dÃ©coupe. Tout vient de `scripts/refonte/` et d'inventaires `awk`
sur le source ; rien n'est estimÃ©. Le suivi du chantier vit dans l'epic #2219.

## Ce que les chiffres changent au plan

- **Un tiers Ã  40 % de chaque fichier est du test**, pas de la production. Le
  monolithe Ã  dÃ©couper est plus petit que sa taille ne le dit, et le module de
  tests inline est lui-mÃªme un fichier gÃ©ant.
- **Une famille manquait au plan v1** (livrÃ©e depuis, #3336) : 22 mÃ©thodes Â« communes Â» de l'orchestrateur,
  appelÃ©es par toutes les autres familles. Elles font une PR Ã  part, en premier.
- **La boucle producteur est presque isolÃ©e** : `play_url` contient
  `build_int_stream` comme fonction imbriquÃ©e, et le branchement par backend vit
  surtout *hors* de ces deux-lÃ .

## Production contre tests

| Fichier | Total | Production | Tests inline |
|---|---:|---:|---:|
| `tune-core/src/orchestrator.rs` | 19 446 | 12 180 | 7 266 |
| `tune-core/src/outputs/local.rs` | 14 607 | 9 230 | 5 377 |
| `tune-core/src/poller.rs` | 10 751 | â 7 150 | â¥ 3 603 |
| `tune-server/src/routes/zones.rs` | 7 446 | â 5 020 | â¥ 2 422 |

Tests inline = modules `#[cfg(test)]` de niveau racine. Pour le poller et les
zones, seuls les modules visibles dans le relevÃ© sont comptÃ©s.

Le module `mod tests` de l'orchestrateur fait **5 811 lignes** Ã  lui seul,
celui de la sortie locale **3 633**. Sortir les tests dans `orchestrator/tests/`
et `local/tests/` est une PR mÃ©canique, sans risque de comportement, qui rend
chaque fichier lisible avant mÃªme de toucher aux familles. Les gardes
d'auto-lecture par `include_str!("orchestrator.rs")` vivent dans ces modules :
elles suivent le dÃ©placement, chemin adaptÃ©, jamais assertion retirÃ©e.

## Orchestrateur

### Anatomie

| Zone | Lignes | Taille | Contenu |
|---|---|---:|---|
| Types et fonctions libres | 1 â 2 063 | 2 063 | `PlaybackOrchestrator`, `PlayRequest`, `ResolvedStream`, `BudgetAdaptatif`, `StreamingDsp`, `ContexteEcoute`, relais de niveaux (`spawn_paced_levels_forwarder` 240 l.), `transcode_source_to_file` 122 l. |
| `impl PlaybackOrchestrator` | 2 064 â 11 408 | 9 344 | 96 mÃ©thodes, les sept familles ci-dessous |
| Auxiliaires | 11 409 â 12 180 | 772 | `BandcampQuality` et helpers |
| Tests | 12 181 â 19 446 | 7 266 | 12 modules, dont `tests` 5 811, `budget_adaptatif_tests` 328, `annonce_apres_sortie_guard` 257 |

### Les familles, mesurÃ©es

Rattachement par motif de nom, Ã  valider Ã  la lecture. Â« RÃ©f. tests Â» =
occurrences du nom de la mÃ©thode dans la zone de tests du fichier.

| Famille | MÃ©thodes | Lignes | RÃ©f. tests | GÃ©ants et remarques |
|---|---:|---:|---:|---|
| **commun** (absente du plan v1, nommÃ©e par cette carte, **livrÃ©e le 04/09 par #3336** : `orchestrator/commun.rs`, 21 mÃ©thodes, `new` reste avec le type) | 22 | 615 | 196 | `new` (147 rÃ©f.), `server_ip`, `reglages_sortie_locale`, `resolve_cover_url`, `message_echec_sortie`, `record_listen`. AppelÃ©e par toutes les autres familles. |
| history | 10 | 242 | 4 | Feuille : un seul appel sortant. ListenBrainz, Last.fm, annonces, tÃ©moins. |
| transport | 21 | 2 201 | 94 | `play_inner` **888**, `seek` 268, `resume` 215, `send_to_output` 186, `stop` 92. La colonne vertÃ©brale, la plus testÃ©e. |
| dsp | 23 | 1 274 | 27 | `refresh_zone_pure_dsp` 98, `set_volume` 95, crossfeed, EQ, niveaux, DoP. AppelÃ©e par les trois rÃ©solveurs. |
| queue | 10 | 835 | 4 | `advance_queue_metadata` 196, `resolve_queue_item_url` 167, `play_from_queue` 129, prÃ©chauffage local et streaming. |
| resolve_direct | 3 | 688 | 12 | `resolve_direct_url` **514**, `resolve_uploaded_file` 99, `dlna_supports_mime` 75. |
| resolve_local | 5 | 1 833 | 12 | `resolve_local_track` **1 692**, passthrough DSD, budget de transcodage. |
| resolve_stream | 2 | 1 655 | 0 | `resolve_streaming_url` **1 594**. **Aucun test inline ne l'appelle** : pas de tÃ©moin dans le fichier. |

### Couplage entre familles

Appels `self.mÃ©thode()` d'une famille vers une autre, comptÃ©s dans les corps.

- **Tout le monde appelle Â« commun Â»** : queue 7, resolve_stream 7,
  resolve_direct 6, transport 6, resolve_local 3, dsp 3.
- **Les rÃ©solveurs appellent dsp** : resolve_stream 9, resolve_local 8. Et
  **dsp appelle transport** 7 fois. Le graphe n'est pas en couches propres.
- **transport appelle history** 6 fois ; history n'appelle presque rien.
- resolve_stream appelle resolve_direct 3 fois et queue 2 fois.

Pour un dÃ©placement pur en sous-modules du mÃªme `impl`, le couplage ne bloque
pas la compilation : les mÃ©thodes restent sur le mÃªme type, en `pub(super)`.
Il dicte l'ordre de lecture et de revue, pas l'ordre de compilation.

### Ordre proposÃ©, rÃ©visÃ© par les chiffres

1. **tests** vers `orchestrator/tests/` : 7 266 lignes, zÃ©ro production
   touchÃ©e, adapte les trois gardes internes.
2. **commun** : 615 lignes, 22 mÃ©thodes appelÃ©es par tous.
3. **history** : 242 lignes, feuille, 4 rÃ©fÃ©rences de test.
4. **queue** puis **resolve_direct** : petites, peu testÃ©es.
5. **dsp** : au milieu du graphe, avant les deux gros rÃ©solveurs qui
   l'appellent 17 fois.
6. **resolve_local** puis **resolve_stream** : un gÃ©ant chacun, dÃ©placÃ© d'un
   bloc sans le toucher. Pour resolve_stream, Ã©crire d'abord un tÃ©moin.
7. **transport** en dernier : la colonne vertÃ©brale reste dans le fichier
   racine tant que les feuilles n'ont pas bougÃ©, et `play_inner` ne se dÃ©coupe
   pas en phase mÃ©canique.

Le plan v2.1 mettait transport en deuxiÃ¨me. Le couplage et les 94 rÃ©fÃ©rences
de test plaident pour le garder au centre jusqu'Ã  la fin.

### Gardes qui lisent `orchestrator.rs`

`audio/crossfeed.rs:699`, `audio/mono_downmix.rs:313` et `:363`, et trois
internes (`12820`, `18881`, `19395`). Les trois internes vivent dans les
modules de tests et suivent leur dÃ©placement.

## Sortie locale : oÃ¹ vit le plan v1

### Anatomie

| Zone | Lignes | Taille | Contenu |
|---|---|---:|---|
| ÃnumÃ©ration et hÃ´tes | 1 â 1 784 | 1 784 | `select_host` 159, `list_asio_devices` 190, `list_audio_devices_uncached` 218, `log_no_devices_diagnostics` 342 |
| `pub struct LocalOutput` + `impl LocalOutput` | 1 785 â 2 506 | 685 | Ãtat de la sortie, constructeur, rÃ©glages |
| DÃ©codage, WAV, ring | 2 507 â 4 607 | 2 100 | `parse_wav_header` 176, `decode_compressed_stream` 110, `ringbuf_tests` 219 |
| `impl OutputTarget for LocalOutput` | 4 608 â 8 355 | 3 747 | 14 mÃ©thodes du trait, dont `play_url` **3 382** |
| RÃ©solution de pÃ©riphÃ©rique | 8 356 â 9 350 | 994 | `resolve_device` 91, `find_device_with_fallback` 168 |
| Tests | 9 351 â 14 607 | 5 377 | 14 modules, dont `tests` 3 633, `backend_fallback_tests` 499, `renseignement_materiel_tests` 289 |

### Le trait tel qu'il est, contre le trait minimal visÃ©

REF-8 vise un backend rÃ©duit Ã  ouvrir, configurer, rendre, observer.

| MÃ©thode | Lignes | RÃ´le dans la cible |
|---|---:|---|
| `play_url` | 3 382 | Tout : nÃ©gociation, dÃ©codage, SRC, DSP, ring, ouverture du flux, callbacks. Contient `build_int_stream<T>` (1 279 l.) comme fonction imbriquÃ©e. C'est la boucle producteur de REF-7. |
| `stop` | 84 | observer / fermer |
| `get_status` | 69 | observer |
| `is_available` | 48 | ouvrir (sonde) |
| `capabilities` | 22 | configurer (nÃ©gociation) |
| `set_mute`, `set_next_url`, `set_next_media`, `dsp_metrics`, `play_media`, `set_volume`, `seek`, `signal_path_status`, `resume` | â¤ 18 | DÃ©lÃ©gations courtes vers l'Ã©tat partagÃ© |

### OÃ¹ est le branchement par backend

| RÃ©gion | windows | asio | linux | macos |
|---|---:|---:|---:|---:|
| Dans `play_url` (4 694 â 8 076) | 6 | 2 | 0 | 3 |
| Reste de la production | 59 | 19 | 11 | 6 |

Onze attributs `cfg` dans les 3 382 lignes de `play_url`, contre 95 dans le
reste : l'Ã©numÃ©ration, la sÃ©lection d'hÃ´te, l'exclusif WASAPI et ASIO vivent
dÃ©jÃ  Ã  part. Ce qui rend la boucle spÃ©cifique passe par `cpal::StreamConfig`
(7 usages), `build_output_stream` (5), `cpal::BufferSize::Default` (3, le sujet
de #3208) et `.play()` (5). Le ring est citÃ© 393 fois dans l'impl, `resample`
71, `gapless` 43, `thread` 37 ; `mlockall` et `SCHED_FIFO` zÃ©ro fois (#3206).

### Gardes qui lisent `local.rs`

Quinze : dix internes, `audio/resample.rs:996`, `tests/dsp_track_boundary.rs`,
et les tests d'intÃ©gration serveur `refus_exclusif_dit_sa_cause_i3108`,
`refus_de_peripherique_partage_dit_pourquoi`, `journal_pcm_alsa_ouvert`,
`echec_de_decodage_dit_pourquoi_i3270`. Toute dÃ©coupe de `play_url` commence
par `scripts/refonte/gardes.sh`.

## Poller et zones, en bref

| Fichier | DÃ©jÃ  fait | Reste | Gardes |
|---|---|---|---|
| `poller.rs` | `mod decisions` 956 l. et `mod fsm` 1 011 l. existent : la moitiÃ© de REF-1 est livrÃ©e depuis juillet. | `impl PositionPoller` 3 620 l., dont `tick` **2 344**, `handle_track_end` 350, `prepare_gapless` 246. `ZonePollState` 155 l. de flags, cible de REF-9. | 4 internes + `tests/poller_bascule.rs` + `tests/octets_servis_inconnus_2394.rs` |
| `routes/zones.rs` | #2769 fusionnÃ©e, plus rien d'ouvert dessus. | `build_signal_path` **760** et `patch_zone` 528 portent la moitiÃ© de la production. `signal_path_tests` 1 580 l. | `zone_manager.rs:1329`, trois internes dont une lit `playback.rs` |

## Trois dÃ©cisions que cette carte appelle

- **Sortir les tests d'abord**, dans les quatre fichiers : 18 000 lignes
  quittent le cÅur sans qu'une ligne de production bouge.
- **Nommer la famille Â« commun Â»** dans l'orchestrateur et la sortir avant
  history.
- **Ãcrire un tÃ©moin pour `resolve_streaming_url`** avant de la dÃ©placer.
  Fait le 4 septembre : `tune-core/tests/temoin_resolution_streaming.rs`, sept
  tests par la porte publique `resolve_queue_item_url` avec un service factice
  (URL http verbatim, https relayÃ©e par le proxy, titre vide, durÃ©e nulle,
  401 rafraÃ®chi une fois, 401 persistant, service inconnu). Contre-Ã©preuve :
  deux sabotages de l'orchestrateur font rougir exactement les deux tests visÃ©s.

## Phase 2 : les grandes fonctions, décomposées sous témoins (5 septembre)

La phase mécanique a déplacé sans rien changer ; la phase 2 découpe les
fonctions géantes en temps nommés, texte copié à l'espace près, chaque temps
prenant en entrée explicite ce qu'il lit et rendant ce qu'il décide. Les
retours anticipés passent par un porteur (`FluxOuFini`, `DecisionOuResolu`,
`ResoluOuFini`, `DashOuFini`) ; les valeurs relevées une fois et lues par
plusieurs temps voyagent dans une struct (`Analyse`, `Habillage`, `Directe`,
`FormatDeSortie`, `DashPret`, `Etiquettes`). Chaque PR est vérifiée sur Shrek
par le comparateur (tests nominatifs, gardes et signatures identiques) et,
quand une garde de texte relit la fonction, par une contre-épreuve compilée.

| Fonction | Avant | Après | Temps | PR |
|---|---:|---:|---|---|
| `routes/zones/signal_path.rs` `build_signal_path` | 760 | 30 | `decrire_la_source`, `relever_les_traitements`, `decider_les_forcages`, `decrire_le_transport`, `rendre_les_verdicts`, `assembler_les_etapes` ; `Analyse` | #3367 à #3374 |
| `routes/zones/ecriture.rs` `patch_zone` | 528 | 3 temps | `valider_le_patch`, `commander_la_sortie`, `persister_le_patch` | #3375 |
| `orchestrator/resolve_stream.rs` `resolve_streaming_url` | 1 588 | ≈ 240 | `resoudre_flux_https`, `resoudre_flux_dash`, `resoudre_flux_local_ou_oaat` ; `FluxOuFini` | #3376, #3377, #3378 |
| `orchestrator/resolve_local.rs` `resolve_local_track` | 1 693 | ≈ 100 | `decider_la_lecture_locale`, `transcoder_la_piste`, `servir_en_passthrough` ; `DecisionLocale`, `DecisionOuResolu` | #3381, #3382 |
| `orchestrator/transport.rs` `play_inner` | 883 | 161 | `resoudre_la_sortie_de_la_zone`, `resoudre_la_demande`, `composer_le_now_playing`, `envoyer_a_la_sortie`, `annoncer_apres_la_sortie`, `arreter_sur_refus_de_sortie` ; `ResoluOuFini`, `Habillage` | #3384 |
| `orchestrator/resolve_local.rs` `transcoder_la_piste` | 852 | 23 | `decider_le_format_de_sortie`, `transcoder_vers_fichier`, `transcoder_en_session` ; `FormatDeSortie`, `FluxLocal` | #3385 |
| `orchestrator/resolve_direct.rs` `resolve_direct_url` | 572 | ≈ 170 | `decoder_la_radio_en_wav`, `decoder_bandcamp_en_wav`, `relayer_bandcamp_au_reseau`, `servir_la_radio_au_reseau` ; `Directe`, `FluxDirect` | #3386 |
| `orchestrator/resolve_stream.rs` `resoudre_flux_https` | 536 | 38 | `pretranscoder_en_flac`, `relayer_le_flux` ; `FluxHttps` | #3387 |
| `orchestrator/resolve_stream.rs` `resoudre_flux_dash` | 573 | 27 | `preparer_le_dash`, `choisir_l_encodage_dash`, `remuxer_le_dash`, `pretranscoder_le_dash` ; `DashPret`, `DashOuFini`, `FluxDash` | #3388 |
| `poller/fin_de_piste.rs` `handle_track_end` | 332 | 40 | `terminer_la_file`, `avancer_avec_reprises` | #3389 |
| `orchestrator/dsp.rs` `serve_prefetched_pcm` | 351 | 41 | `etiqueter_le_prefetch`, `encoder_le_prefetch_en_fichier`, `servir_le_prefetch_en_wav` ; `Etiquettes` | #3390 |
| `poller/fin_de_piste.rs` `prepare_gapless` | 290 | 35 | `armer_le_fichier_local`, `armer_le_flux_suivant` | #3391 |
| `orchestrator/transport.rs` `seek` | 276 | 55 | `deplacer_la_sortie` | #3392 |
| `orchestrator/resolve_local.rs` `decider_la_lecture_locale` | 549 | ≈ 430 | `anticiper_le_dop` | PR à suivre |

### Gardes de texte suivies pendant la phase 2

- `annonce_apres_sortie_guard` : les quatre motifs (`let (output_sent, output_error) =`,
  `if output_sent {\n            self.dispatch_now_playing(`, `if output_sent && record_history`,
  `if !output_sent && zone_navigateur {`) sont intacts et gardent leur ordre ; la
  fenêtre de `le_scrobble_definitif_reste_hors_du_demarrage` s'étend désormais de
  `play_inner` à `recreate_local_and_play`, pour couvrir les six temps
  (contre-épreuves : `dispatch_scrobble(` dans le cinquième temps → rouge ; juste
  après la borne → vert).
- La tranche « niveaux sur cache hit » (`"transcode_cache_hit"` →
  `"transcode_to_temp_file_start"`) reste entière dans `transcoder_vers_fichier`.
- `resolution_annoncee_tests` relit `sample_rate: resolution_annoncee(` au même
  retrait dans `composer_le_now_playing`.

### Ce qui reste grand, et pourquoi on ne le coupe pas au texte

| Fonction | Lignes | Raison |
|---|---:|---|
| `poller/tick.rs` `tick` | 2 402 | REF-9 : machine à états, pas un découpage |
| `orchestrator/radio.rs` `decode_radio_stream_to_pcm` | 394 | une boucle `'reconnect` à `continue`/`break` étiquetés |
| `orchestrator/resolve_stream.rs` `resoudre_flux_local_ou_oaat` | 342 | 210 lignes sont une tâche `tokio::spawn` : la sortir, c'est reprendre ses captures |
| `routes/zones/ecriture.rs` `persister_le_patch` | 354 | trente écritures plates sous la macro `ecrire!`, lues par `patch_zone_error_guard` |
| `outputs/local.rs` `play_url` | 3 382 | REF-6/REF-7 |

## Refaire le relevÃ©

```bash
scripts/refonte/gardes.sh HEAD releves/gardes.txt
scripts/refonte/empreinte-api.sh releves HEAD
scripts/refonte/tests-nominatifs.sh releves          # sur Shrek
```

Les inventaires de fonctions par fichier sont un `awk` d'une ligne sur les
`fn` par indentation ; ils ne sont pas versionnÃ©s parce qu'ils se refont en
une seconde et changent Ã  chaque commit.
