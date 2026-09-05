# Carte de découpe du cœur

Relevé sur `main` v0.9.133 (`9c7c6d80`), 4 septembre 2026. Anatomie mesurée
des quatre fichiers du cœur, familles de l'orchestrateur avec leur couplage,
cibles du plan v1 (#2219) dans la sortie locale, et les gardes qui cassent à
la première découpe. Tout vient de `scripts/refonte/` et d'inventaires `awk`
sur le source ; rien n'est estimé. Le suivi du chantier vit dans l'epic #2219.

## Ce que les chiffres changent au plan

- **Un tiers à 40 % de chaque fichier est du test**, pas de la production. Le
  monolithe à découper est plus petit que sa taille ne le dit, et le module de
  tests inline est lui-même un fichier géant.
- **Une famille manquait au plan v1** (livrée depuis, #3336) : 22 méthodes « communes » de l'orchestrateur,
  appelées par toutes les autres familles. Elles font une PR à part, en premier.
- **La boucle producteur est presque isolée** : `play_url` contient
  `build_int_stream` comme fonction imbriquée, et le branchement par backend vit
  surtout *hors* de ces deux-là.

## Production contre tests

| Fichier | Total | Production | Tests inline |
|---|---:|---:|---:|
| `tune-core/src/orchestrator.rs` | 19 446 | 12 180 | 7 266 |
| `tune-core/src/outputs/local.rs` | 14 607 | 9 230 | 5 377 |
| `tune-core/src/poller.rs` | 10 751 | ≈ 7 150 | ≥ 3 603 |
| `tune-server/src/routes/zones.rs` | 7 446 | ≈ 5 020 | ≥ 2 422 |

Tests inline = modules `#[cfg(test)]` de niveau racine. Pour le poller et les
zones, seuls les modules visibles dans le relevé sont comptés.

Le module `mod tests` de l'orchestrateur fait **5 811 lignes** à lui seul,
celui de la sortie locale **3 633**. Sortir les tests dans `orchestrator/tests/`
et `local/tests/` est une PR mécanique, sans risque de comportement, qui rend
chaque fichier lisible avant même de toucher aux familles. Les gardes
d'auto-lecture par `include_str!("orchestrator.rs")` vivent dans ces modules :
elles suivent le déplacement, chemin adapté, jamais assertion retirée.

## Orchestrateur

### Anatomie

| Zone | Lignes | Taille | Contenu |
|---|---|---:|---|
| Types et fonctions libres | 1 – 2 063 | 2 063 | `PlaybackOrchestrator`, `PlayRequest`, `ResolvedStream`, `BudgetAdaptatif`, `StreamingDsp`, `ContexteEcoute`, relais de niveaux (`spawn_paced_levels_forwarder` 240 l.), `transcode_source_to_file` 122 l. |
| `impl PlaybackOrchestrator` | 2 064 – 11 408 | 9 344 | 96 méthodes, les sept familles ci-dessous |
| Auxiliaires | 11 409 – 12 180 | 772 | `BandcampQuality` et helpers |
| Tests | 12 181 – 19 446 | 7 266 | 12 modules, dont `tests` 5 811, `budget_adaptatif_tests` 328, `annonce_apres_sortie_guard` 257 |

### Les familles, mesurées

Rattachement par motif de nom, à valider à la lecture. « Réf. tests » =
occurrences du nom de la méthode dans la zone de tests du fichier.

| Famille | Méthodes | Lignes | Réf. tests | Géants et remarques |
|---|---:|---:|---:|---|
| **commun** (absente du plan v1, nommée par cette carte, **livrée le 04/09 par #3336** : `orchestrator/commun.rs`, 21 méthodes, `new` reste avec le type) | 22 | 615 | 196 | `new` (147 réf.), `server_ip`, `reglages_sortie_locale`, `resolve_cover_url`, `message_echec_sortie`, `record_listen`. Appelée par toutes les autres familles. |
| history | 10 | 242 | 4 | Feuille : un seul appel sortant. ListenBrainz, Last.fm, annonces, témoins. |
| transport | 21 | 2 201 | 94 | `play_inner` **888**, `seek` 268, `resume` 215, `send_to_output` 186, `stop` 92. La colonne vertébrale, la plus testée. |
| dsp | 23 | 1 274 | 27 | `refresh_zone_pure_dsp` 98, `set_volume` 95, crossfeed, EQ, niveaux, DoP. Appelée par les trois résolveurs. |
| queue | 10 | 835 | 4 | `advance_queue_metadata` 196, `resolve_queue_item_url` 167, `play_from_queue` 129, préchauffage local et streaming. |
| resolve_direct | 3 | 688 | 12 | `resolve_direct_url` **514**, `resolve_uploaded_file` 99, `dlna_supports_mime` 75. |
| resolve_local | 5 | 1 833 | 12 | `resolve_local_track` **1 692**, passthrough DSD, budget de transcodage. |
| resolve_stream | 2 | 1 655 | 0 | `resolve_streaming_url` **1 594**. **Aucun test inline ne l'appelle** : pas de témoin dans le fichier. |

### Couplage entre familles

Appels `self.méthode()` d'une famille vers une autre, comptés dans les corps.

- **Tout le monde appelle « commun »** : queue 7, resolve_stream 7,
  resolve_direct 6, transport 6, resolve_local 3, dsp 3.
- **Les résolveurs appellent dsp** : resolve_stream 9, resolve_local 8. Et
  **dsp appelle transport** 7 fois. Le graphe n'est pas en couches propres.
- **transport appelle history** 6 fois ; history n'appelle presque rien.
- resolve_stream appelle resolve_direct 3 fois et queue 2 fois.

Pour un déplacement pur en sous-modules du même `impl`, le couplage ne bloque
pas la compilation : les méthodes restent sur le même type, en `pub(super)`.
Il dicte l'ordre de lecture et de revue, pas l'ordre de compilation.

### Ordre proposé, révisé par les chiffres

1. **tests** vers `orchestrator/tests/` : 7 266 lignes, zéro production
   touchée, adapte les trois gardes internes.
2. **commun** : 615 lignes, 22 méthodes appelées par tous.
3. **history** : 242 lignes, feuille, 4 références de test.
4. **queue** puis **resolve_direct** : petites, peu testées.
5. **dsp** : au milieu du graphe, avant les deux gros résolveurs qui
   l'appellent 17 fois.
6. **resolve_local** puis **resolve_stream** : un géant chacun, déplacé d'un
   bloc sans le toucher. Pour resolve_stream, écrire d'abord un témoin.
7. **transport** en dernier : la colonne vertébrale reste dans le fichier
   racine tant que les feuilles n'ont pas bougé, et `play_inner` ne se découpe
   pas en phase mécanique.

Le plan v2.1 mettait transport en deuxième. Le couplage et les 94 références
de test plaident pour le garder au centre jusqu'à la fin.

### Gardes qui lisent `orchestrator.rs`

`audio/crossfeed.rs:699`, `audio/mono_downmix.rs:313` et `:363`, et trois
internes (`12820`, `18881`, `19395`). Les trois internes vivent dans les
modules de tests et suivent leur déplacement.

## Sortie locale : où vit le plan v1

### Anatomie

| Zone | Lignes | Taille | Contenu |
|---|---|---:|---|
| Énumération et hôtes | 1 – 1 784 | 1 784 | `select_host` 159, `list_asio_devices` 190, `list_audio_devices_uncached` 218, `log_no_devices_diagnostics` 342 |
| `pub struct LocalOutput` + `impl LocalOutput` | 1 785 – 2 506 | 685 | État de la sortie, constructeur, réglages |
| Décodage, WAV, ring | 2 507 – 4 607 | 2 100 | `parse_wav_header` 176, `decode_compressed_stream` 110, `ringbuf_tests` 219 |
| `impl OutputTarget for LocalOutput` | 4 608 – 8 355 | 3 747 | 14 méthodes du trait, dont `play_url` **3 382** |
| Résolution de périphérique | 8 356 – 9 350 | 994 | `resolve_device` 91, `find_device_with_fallback` 168 |
| Tests | 9 351 – 14 607 | 5 377 | 14 modules, dont `tests` 3 633, `backend_fallback_tests` 499, `renseignement_materiel_tests` 289 |

### Le trait tel qu'il est, contre le trait minimal visé

REF-8 vise un backend réduit à ouvrir, configurer, rendre, observer.

| Méthode | Lignes | Rôle dans la cible |
|---|---:|---|
| `play_url` | 3 382 | Tout : négociation, décodage, SRC, DSP, ring, ouverture du flux, callbacks. Contient `build_int_stream<T>` (1 279 l.) comme fonction imbriquée. C'est la boucle producteur de REF-7. |
| `stop` | 84 | observer / fermer |
| `get_status` | 69 | observer |
| `is_available` | 48 | ouvrir (sonde) |
| `capabilities` | 22 | configurer (négociation) |
| `set_mute`, `set_next_url`, `set_next_media`, `dsp_metrics`, `play_media`, `set_volume`, `seek`, `signal_path_status`, `resume` | ≤ 18 | Délégations courtes vers l'état partagé |

### Où est le branchement par backend

| Région | windows | asio | linux | macos |
|---|---:|---:|---:|---:|
| Dans `play_url` (4 694 – 8 076) | 6 | 2 | 0 | 3 |
| Reste de la production | 59 | 19 | 11 | 6 |

Onze attributs `cfg` dans les 3 382 lignes de `play_url`, contre 95 dans le
reste : l'énumération, la sélection d'hôte, l'exclusif WASAPI et ASIO vivent
déjà à part. Ce qui rend la boucle spécifique passe par `cpal::StreamConfig`
(7 usages), `build_output_stream` (5), `cpal::BufferSize::Default` (3, le sujet
de #3208) et `.play()` (5). Le ring est cité 393 fois dans l'impl, `resample`
71, `gapless` 43, `thread` 37 ; `mlockall` et `SCHED_FIFO` zéro fois (#3206).

### Gardes qui lisent `local.rs`

Quinze : dix internes, `audio/resample.rs:996`, `tests/dsp_track_boundary.rs`,
et les tests d'intégration serveur `refus_exclusif_dit_sa_cause_i3108`,
`refus_de_peripherique_partage_dit_pourquoi`, `journal_pcm_alsa_ouvert`,
`echec_de_decodage_dit_pourquoi_i3270`. Toute découpe de `play_url` commence
par `scripts/refonte/gardes.sh`.

## Poller et zones, en bref

| Fichier | Déjà fait | Reste | Gardes |
|---|---|---|---|
| `poller.rs` | `mod decisions` 956 l. et `mod fsm` 1 011 l. existent : la moitié de REF-1 est livrée depuis juillet. | `impl PositionPoller` 3 620 l., dont `tick` **2 344**, `handle_track_end` 350, `prepare_gapless` 246. `ZonePollState` 155 l. de flags, cible de REF-9. | 4 internes + `tests/poller_bascule.rs` + `tests/octets_servis_inconnus_2394.rs` |
| `routes/zones.rs` | #2769 fusionnée, plus rien d'ouvert dessus. | `build_signal_path` **760** et `patch_zone` 528 portent la moitié de la production. `signal_path_tests` 1 580 l. | `zone_manager.rs:1329`, trois internes dont une lit `playback.rs` |

## Trois décisions que cette carte appelle

- **Sortir les tests d'abord**, dans les quatre fichiers : 18 000 lignes
  quittent le cœur sans qu'une ligne de production bouge.
- **Nommer la famille « commun »** dans l'orchestrateur et la sortir avant
  history.
- **Écrire un témoin pour `resolve_streaming_url`** avant de la déplacer.
  Fait le 4 septembre : `tune-core/tests/temoin_resolution_streaming.rs`, sept
  tests par la porte publique `resolve_queue_item_url` avec un service factice
  (URL http verbatim, https relayée par le proxy, titre vide, durée nulle,
  401 rafraîchi une fois, 401 persistant, service inconnu). Contre-épreuve :
  deux sabotages de l'orchestrateur font rougir exactement les deux tests visés.

## Refaire le relevé

```bash
scripts/refonte/gardes.sh HEAD releves/gardes.txt
scripts/refonte/empreinte-api.sh releves HEAD
scripts/refonte/tests-nominatifs.sh releves          # sur Shrek
```

Les inventaires de fonctions par fichier sont un `awk` d'une ligne sur les
`fn` par indentation ; ils ne sont pas versionnés parce qu'ils se refont en
une seconde et changent à chaque commit.
