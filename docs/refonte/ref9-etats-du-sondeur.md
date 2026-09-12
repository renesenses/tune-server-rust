# REF-9 préparatoire — les états du sondeur

Epic #2219, exigence « état de lecture et transitions exprimés par machine à
états, pas par accumulation de flags ». Ce document ne décide rien : il relève
ce que `tune-core/src/poller/tick.rs` fait aujourd'hui de `ZonePollState`, pour
que la machine à états se dessine sur des faits. Relevé du 12/09/2026 sur
`batch/bugs-11` à `72fbdf86`.

Mesuré :

- `tune-core/src/poller/etat.rs` : `ZonePollState`, **39 champs**, dont
  **5 booléens**, 9 `Option`, 2 `Instant` nus, 21 entiers (compteurs et caches),
  2 structures de constat (`journal`, `famine`).
- `tune-core/src/poller/tick.rs` : 2 545 lignes, une seule fonction `tick`
  (l. 4 à 2 544). **163 sites d'écriture** sur les 39 champs, tous dans `tick`
  (aucun dans `fin_de_piste.rs`, `radio.rs` ni `poller.rs`). `gapless_sent`
  seul est écrit à 11 endroits.
- Aucun `debug_assert!`, aucun `unreachable!`, aucun commentaire « impossible »
  dans le sondeur : les combinaisons interdites sont toutes implicites.

## Ce que `fsm.rs` et `decisions.rs` couvrent déjà

`poller/fsm.rs` (987 l.) est un **classifieur pur en ombre**, pas une machine :

- `classify_stopped(&StoppedInput) -> StoppedOutcome` reproduit l'arbre de
  décision du bras `TransportState::Stopped` (16 issues, ordre des branches
  identique à `tick`). `classify_playing(&PlayingInput) -> PlayingDecision`
  reproduit les quatre décisions du bras `Playing` (confirmer l'avance,
  transition détectée, armer, position au-delà de la fin).
- Il ne lit pas `ZonePollState` : `tick` recopie les champs dans `StoppedInput`
  (l. 1437-1471) et `PlayingInput` (l. 1878-1890), appelle le classifieur
  **après** avoir muté l'état, et compare (`POLLER_FSM_SHADOW`, l. 1850 et
  2429, journal `poller_fsm_shadow_divergence`). Il ne pilote aucune écriture.
- `ConsommationFlux` (trois états, #2394) est le seul morceau de `fsm.rs` que
  `tick` consomme réellement (l. 1752).

`poller/decisions.rs` (1 277 l.) porte **39 prédicats purs** (`pub fn`) — fenêtre
d'armement, fin naturelle, remise à zéro de position, décrochage, famine,
tenue du renderer — et deux types d'état (`SuiviFamine`, `TenueDuRenderer`).

Ce qu'aucun des deux ne couvre :

- **les écritures** : quel champ change, sur quelle issue, dans quel ordre —
  c'est tout `tick`, et c'est ce que ce document relève ;
- la **naissance et la mort** de l'état (`poll_states.retain / insert /
  remove`, sept sites) ;
- le chemin **radio** (l. 828-1034), le chemin **sans périphérique**
  (l. 346-396), le chemin **sonde en échec** (l. 650-716) et la **zone au
  repos** (l. 26-319) : quatre boucles sans classifieur ;
- les **verrous par piste** (`tenue_signalee`, `depassement_duree_signale`,
  `wall_clock_end_fired`, `scrobbled_key`, `gapless_dsd_skip_pos`) : aucun
  n'entre dans `StoppedInput` ni `PlayingInput`, hormis `wall_clock_end_fired`
  passé à `poll_failed_past_end`.

## Les 39 champs, et où ils s'écrivent

Rôles : **identité** (de zone ou de piste), **horloge** (`Instant`),
**drapeau** (booléen d'état), **verrou** (drapeau ou option qui ne se lève
qu'une fois par piste), **cache** (dernière valeur vue), **compteur**,
**constat** (structure de journal, sans décision). `etat.rs:182-224` (`new`)
initialise tout ; il n'est pas répété ligne à ligne ci-dessous.

| # | Champ | Type | Rôle | Écritures (`tick.rs:`) |
|--:|---|---|---|---|
| 1 | `gapless_sent` | `bool` | drapeau | `=false` 443, 1308, 1515, 1663, 1697, 2002, 2057, 2100 · `=true` 2147 (sortie exclusive : suppresseur de ré-armement, rien n'est envoyé), 2165 (`SetNext` accepté) |
| 2 | `stopped_ticks` | `u8` | compteur | `=0` 446, 1313, 1396, 1475, 1483, 1493, 1518, 1542, 1588, 1666, 1860, 2012, 2442 · `+=1` 1595 |
| 3 | `tenue_etrangere_ticks` | `u8` | compteur | `+1` 181 · `=0` 215, 448 |
| 4 | `tenue_signalee` | `bool` | verrou | `=true` 183 · `=false` 449 |
| 5 | `gapless_cooldown` | `u8` | compteur | `=4` 1354, 1524, 1672, 1911, 2036 · `-=1` 1492 · `=0` 445, 1861 |
| 6 | `consecutive_errors` | `u8` | compteur | `=0` 629 · `+1` 651 |
| 7 | `backoff_remaining` | `u8` | compteur | `-=1` 504 · `=1<<min(n,4)` 653 |
| 8 | `journal` | `JournalSondage` | constat | `.succes_lecture` 632 · `.echec_lecture` 664 (par `&mut`) |
| 9 | `total_polls` | `u64` | compteur | `+=1` 528 |
| 10 | `total_errors` | `u64` | compteur | `+=1` 652 |
| 11 | `last_latency_ms` | `u32` | cache | 634 |
| 12 | `max_latency_ms` | `u32` | cache | 636 |
| 13 | `last_radio_poll` | `Instant` | horloge | 358 (zone sans périphérique), 821 |
| 14 | `gapless_sent_at` | `Option<Instant>` | horloge | `=Some` 2164 · `=None` 444, 1309, 1368 (garde de 15 s expirée), 1517, 1665, 2003, 2058, 2101 |
| 15 | `last_position_ms` | `u64` | cache | 1283 · `=0` 434, 1316, 1520, 1668, 2007 |
| 16 | `peak_position_ms` | `u64` | cache (plafond) | 1205 · `=0` 435, 1315, 1519, 1667, 2006 |
| 17 | `scrobbled_key` | `Option<String>` | verrou | `=Some` 499 · `=None` 436, 1359, 1914, 2039 |
| 18 | `ticks_since_db_save` | `u64` | compteur | `+=1` 1154 · `=0` 1156 |
| 19 | `track_started_at` | `Option<Instant>` | horloge | `=Some(now)` 441, 1320, 1918, 2011 · repli au déplacement 1084-1085 · `=None` 1521, 1669 |
| 20 | `last_seek_seen` | `Option<Instant>` | cache (identité du dernier déplacement) | 1082 |
| 21 | `track_generation` | `u64` | identité | 447 |
| 22 | `track_loaded_at` | `Instant` | horloge | 450 |
| 23 | `past_end_ticks` | `u8` | compteur | `+=1` 2288 · `=0` 440, 451, 1314, 2013, 2307 |
| 24 | `gapless_advance_pending` | `bool` | drapeau | `=true` 1522, 1670 · `=false` 452, 1321, 1540, 1900, 2014 |
| 25 | `gapless_stuck_ticks` | `u8` | compteur | `+=1` 1532 · `=0` 453, 1322, 1523, 1541, 1671, 1901, 2015 |
| 26 | `last_bytes_sent` | `u64` | cache | 1759, 2407 · `=0` 437, 1317, 2008 |
| 27 | `playing_stall_ticks` | `u8` | compteur | 2399 (`next_dlna_playing_stall_ticks`) · `=0` 438, 1318, 1397, 1400, 2009, 2423, 2426, 2443 |
| 28 | `depassement_duree_ticks` | `u8` | compteur | `+1` 2333 · `=0` 457, 2335 |
| 29 | `depassement_duree_signale` | `bool` | verrou | `=true` 2342 · `=false` 458, 2336 |
| 30 | `stall_declines` | `u8` | compteur | `+1` 1706 · `=0` 439, 1319, 2010 |
| 31 | `radio_stopped_ticks` | `u8` | compteur | `+1` 916 · `=0` 842, 943 |
| 32 | `last_radio_position_ms` | `u64` | cache | 837 |
| 33 | `last_device_volume` | `Option<f64>` | cache | 130 (zone au repos), 873 (radio), 1151 |
| 34 | `wall_clock_end_fired` | `bool` | verrou | `=true` 712 · `=false` 455 |
| 35 | `gapless_arm_logged` | `Option<bool>` | cache (diagnostic seul) | 1964 · `=None` 460, 2018, 2107 |
| 36 | `gapless_dsd_skip_pos` | `Option<i64>` | verrou | `=Some` 2172 · `=None` 461, 2019 |
| 37 | `gapless_armed` | `Option<ArmedNext>` | donnée d'état (ce que le renderer a accepté) | `=Some` 2169 · `.take()` 1312, 2005 · `=None` 462, 1516, 1664, 1698, 2059, 2102, 2151 |
| 38 | `famine` | `SuiviFamine` | constat | `.observer` 749 · `.reinitialiser` 755 (par `&mut`) |
| 39 | `famine_releve_at` | `Option<Instant>` | horloge | `=Some` 748 · `=None` 756 |

Naissance et mort de l'état (ce sont aussi des transitions, et les plus
brutales) :

| Site | Ce qui se passe |
|---|---|
| `tick.rs:12-16` | `retain` : toute zone qui n'est plus `Playing` côté Tune perd son état, sans journal |
| `tick.rs:349`, `:401` | `or_insert_with(ZonePollState::new)` : zone sans périphérique, zone avec |
| `tick.rs:393` | zone navigateur abandonnée (`abandonner_lecture_sans_destination`) |
| `tick.rs:610` | la sortie a rapporté une panne : `remove` + `orchestrator.stop` |
| `tick.rs:1020` | radio : six ticks `Stopped` sans reprise, `remove` + `stop` |
| `tick.rs:2468` | `force_stop` : `remove`, puis `stop` ou relance « démarrage mort » |
| `tick.rs:2540` | `track_ended` : `remove`, puis `handle_track_end` |

Le chemin « sonde en échec » (l. 650-716) ne peut pas retirer l'état — il tient
l'emprunt — d'où le verrou `wall_clock_end_fired` (etat.rs:122-126) : c'est le
seul cas où `handle_track_end` est appelé avec un état de sondage qui survit.

## Combinaisons que le code suppose impossibles

Aucune n'est vérifiée par une assertion ; toutes tiennent à l'ordre des
écritures.

| Combinaison | Pourquoi elle est censée ne pas exister | Où ça se joue |
|---|---|---|
| `gapless_sent && gapless_advance_pending` | chaque `pending = true` (1522, 1670) est précédé de `sent = false` (1515, 1663) ; chaque `sent = true` (2147, 2165) est dans le bras `Playing`, qui a rabattu `pending` à 1900 | invariant implicite |
| `gapless_armed.is_some() && !gapless_sent` | posés ensemble (2165-2169), retirés ensemble ; 2151 pose `sent` sans `armed` (sortie exclusive), jamais l'inverse | invariant implicite |
| `gapless_sent_at.is_some() && !gapless_sent` | 1697 rabat `sent` **sans** rabattre `sent_at` — mais l'état est retiré 800 lignes plus bas (2540) le même tour : écriture morte, pas incohérence | `tick.rs:1697` |
| `gapless_sent && gapless_sent_at.is_none()` | **possible et voulu** : garde de 15 s expirée (1368) ou sortie exclusive (2147). `gapless_stage_expired` s'en protège (`staged_without_a_timestamp_is_left_alone`) | deux sens pour un même drapeau |
| `gapless_advance_pending && gapless_cooldown == 0 && track_loaded_at < 45 s && peak < 5 s` | après 1515-1524, `peak = 0` ; si la piste a été chargée il y a moins de 45 s, la grâce de chargement (1411-1413) **masque** le détecteur de blocage (1526) : `SuppressLoadGrace` avant `StuckWaiting`. Concerne une piste courte enchaînée en gapless | ordre des branches, `fsm.rs:199-218` |
| `track_started_at.is_none()` en `Playing` | transitoire : 1521/1669 le vident, 1918 le repose au premier `Playing` ; entre les deux, `wall_elapsed = 0` pour toutes les gardes | `tick.rs:1917-1919` |
| `tenue_signalee && tenue_etrangere_ticks < 3` | impossible par construction (183 sous `>= 3`) ; les deux sont remis ensemble à 448-449 | — |
| `depassement_duree_signale && depassement_duree_ticks < 60` | idem (2342 sous `>= 60`), remis ensemble à 2335-2336 | — |
| `wall_clock_end_fired` hors chemin « sonde en échec » | posé à 712 seulement ; lu seulement par `poll_failed_past_end` ; pas remis avant le changement de génération (455) — l'état survit à `handle_track_end` avec ce verrou levé | `etat.rs:122-126` |
| `track_ended && force_stop` | 1729 et 1806 écrivent `track_ended = false` avant `force_stop = true` alors que rien ne l'avait levé : défensif, et `if force_stop … else if track_ended` (2467, 2512) tranche de toute façon | branches défensives |
| `classify_stopped ≠ fsm_actual` | admis possible : c'est l'objet de `poller_fsm_shadow_divergence` (1850-1856, 2429-2438), sous drapeau | la seule « assertion » du sondeur, et elle ne fait que journaliser |

Un drapeau à deux sens : `gapless_sent` vaut « `SetNext` est parti » (2165)
**et** « on a renoncé à armer, ne plus réessayer » (2147, DLNA-DSD via
`gapless_dsd_skip_pos` pour la même raison, etat.rs:133-143). Sur DLNA, le
premier sens active les détecteurs de transition (`duration_changed`,
`position_reset`) ; le second ne doit pas. C'est `can_internal_gapless`,
re-sondé à chaque usage (1263-1269, 1642-1648, 2138-2144, 2236-2242 — quatre
verrous de sortie par tour dans le pire cas), qui les départage.

## Les transitions que `tick` prend réellement

Pour chaque écriture d'un drapeau d'état : la condition (une ligne) et ce qui
devient visible dehors. « Journal » = ligne `tracing` ; « NowPlaying » = appel
à l'orchestrateur qui change ce que l'écran affiche ; « événement » = bus.

| Ligne | Écriture | Condition | Visible dehors |
|---|---|---|---|
| 443-462 | tout rabattu (5 drapeaux, 7 options) | `track_generation` a changé (play / next / previous) ; si déplacement < 10 s, la position et le pic survivent | journal `poller_track_generation_changed_resetting_state` |
| 2165 | `gapless_sent = true`, `sent_at`, `armed` | `Playing`, position ≥ durée − 30 s, non armé, gapless activé sur la zone, sortie capable, pas de verrou DSD, `prepare_gapless` → `Armed` | `SetNextAVTransportURI` envoyé (journal dans `prepare_gapless`) ; NowPlaying inchangé |
| 2147 | `gapless_sent = true`, `armed = None` | idem, sortie **sans** enchaînement interne | journal `gapless_skipped_exclusive_output` ; rien n'est envoyé |
| 2057 | `sent = false`, `sent_at`, `armed` | armé depuis > 200 s (`gapless_stage_expired`) | journal `gapless_stage_expired_rearming` ; ré-armement dans le même tour |
| 2100 | `sent = false`, `sent_at`, `armed`, `arm_logged` | la ligne de file armée n'est plus la suivante (`gapless_arm_outdated`, #3026) | journal `gapless_rearm_queue_changed` ; nouveau `SetNext` dans le même tour |
| 2002-2039 | `sent = false`, `armed.take()`, compteurs de piste à 0, `cooldown = 4`, `scrobbled_key = None` | `Playing`, durée rapportée ≠ durée de file (> 2 s) **et** position < 5 s ou ≥ durée − 30 s, assez joué | journal `gapless_transition_detected` ; NowPlaying avance (`advance_queue_metadata`) ou `handle_track_end` si pas de suivante |
| 1308-1359 | idem, sans `arm_logged` | `Playing`, position passée de > 30 s à < 5 s, armé, sortie capable, hors grâce de déplacement, assez joué, pas de relance < 20 s | journal `gapless_position_reset_detected` ; NowPlaying avance |
| 1515-1524 | `sent = false`, `pending = true`, `cooldown = 4`, `track_started_at = None`, pic à 0 | `Stopped`, dans la garde de 15 s après `SetNext`, assez joué | journal `gapless_guard_stopped_pending_confirmation` ; rien dehors |
| 1663-1672 | idem | `Stopped` × 5, fin naturelle, armé, sortie capable | journal `gapless_natural_end_waiting_for_transition` ; rien dehors |
| 1900-1914 | `pending = false`, `stuck = 0`, `cooldown = 4`, `scrobbled_key = None` | `Playing` alors que `pending` | journal `gapless_confirmed_advancing_metadata` ; NowPlaying avance |
| 1540-1544 | `pending = false`, `stuck = 0` → `track_ended` | `Stopped`, `pending`, refroidissement écoulé, 2 ticks bloqués | journal `gapless_advance_stuck_forcing_play` puis `track_end_gap` ; `handle_track_end` → `play_from_queue` |
| 1697-1701 | `sent = false`, `armed = None` → `track_ended` | `Stopped` × 5, fin naturelle, non armé ou sortie sans enchaînement, ≥ 90 % du flux servi | journal `track_end_gap` (motif `natural_end_after_stopped`) ; `handle_track_end` |
| 1571, 1589, 2302 | `track_ended` (sans drapeau) | `ended_naturally` plausible ; DSD sur DLNA au pic ; position > durée + 3 s pendant 3 ticks (ou horloge murale, DLNA / Chromecast / DMP figé) | journal `local_output_ended_naturally_advancing` / `dlna_dsd_reached_end_advancing` / `position_past_end_advancing`, puis `track_end_gap` |
| 1730, 1807, 2418 | `force_stop` (sans drapeau) | 10 refus de fin sur flux incomplet ; `Stopped` × 30 sans fin et compteur d'octets **mesuré** à sec ; `Playing` × 30 sans progrès ni octets | journal `renderer_stalled…` / `playback_failure_stopping_zone` / `dlna_playing_without_progress_stopping_zone` ; `orchestrator.stop`, ou Pause→Stop→Play si « démarrage mort » DLNA |
| 712 | `wall_clock_end_fired = true` | sonde en **erreur**, DLNA, ≥ 2 erreurs consécutives, horloge ≥ durée + 3 s, hors grâce | journal `dlna_poll_failed_wall_clock_advancing` ; `handle_track_end` (l'état survit) |
| 183 | `tenue_signalee = true` | zone au repos côté Tune, `Playing` chez le renderer, URI étrangère 3 ticks | journal `renderer_tenu_par_un_tiers` ; événement `zone.playback_error` fatal |
| 2342 | `depassement_duree_signale = true` | `Playing`, position ≥ durée − 3 s pendant 60 ticks, aucun détecteur n'a conclu | journal `lecture_annoncee_au_dela_de_la_duree` ; métrique `lecture_au_dela_de_la_duree` — **aucune action** (#2493) |
| 916-1020 | `radio_stopped_ticks` | radio : `Stopped` **et** position qui n'avance pas ; 3 à 5 ticks → relance sans historique ; ≥ 6 ou station déjà refusée → `remove` + `stop` | journal `radio_auto_retry` / `radio_renderer_stopped_giving_up` |
| 651-653 | `consecutive_errors`, `backoff_remaining` | sonde en erreur | journal plafonné (`JournalSondage`), zone non sondée 2^n ticks |

Transitions **non témoignables sans sonde** : 2165 et 2147 dépendent de
`prepare_gapless` et de `supports_internal_gapless()` (verrou de sortie) ; 183
de `status.current_uri` d'un renderer ; 1697 des octets servis
(`streamer_bytes_sent`) ; 712 d'un `Err` de sonde. Les témoins prennent le
prédicat qui décide et le miroir des écritures, pas la sonde.

## Proposition d'énumération — pour l'arbitrage, pas une décision

> Ceci est une PROPOSITION. Le dessin de la machine appartient à Bertrand ;
> les faits ci-dessus sont là pour qu'il tranche sur pièces. Trois points
> restent ouverts, listés après la table.

Huit variantes. Les champs qui n'appartiennent qu'à un état vont dans sa
variante ; ce qui vaut pour toute la vie de la zone reste dans
`ZonePollState`, à côté de l'enum.

| Variante | Sens | Champs portés (aujourd'hui : # de la table) |
|---|---|---|
| `Neuve` | piste chargée, aucun échantillon honnête encore (grâce de 45 s, `stale_start_position`) | 22 `track_loaded_at` |
| `Lecture` | le renderer joue, rien d'armé | 19 `track_started_at`, 16 `peak`, 15 `last_position`, 23 `past_end_ticks`, 27 `playing_stall_ticks`, 26 `last_bytes_sent`, 36 `gapless_dsd_skip_pos`, 35 `gapless_arm_logged` |
| `Armee` | `SetNext` accepté (ou renoncé sur sortie exclusive — sous-variante ou champ `exclusive: bool`) | ceux de `Lecture` + 14 `gapless_sent_at`, 37 `gapless_armed` ; **1 `gapless_sent` disparaît** (c'est la variante) |
| `Arretee` | le renderer dit `Stopped`, Tune joue ; on compte | ceux de `Lecture` ou `Armee` + 2 `stopped_ticks`, 30 `stall_declines` |
| `AvancePendante` | on attend que le renderer rejoue pour confirmer l'enchaînement | 5 `gapless_cooldown`, 25 `gapless_stuck_ticks` ; **24 `gapless_advance_pending` disparaît** |
| `Radio` | flux sans fin : ni pic, ni fin, ni gapless | 31 `radio_stopped_ticks`, 32 `last_radio_position_ms`, 13 `last_radio_poll` |
| `SondageEnEchec` | la sonde ne répond pas ; recul exponentiel | 6 `consecutive_errors`, 7 `backoff_remaining` ; **34 `wall_clock_end_fired` disparaît** (devient `fin_prononcee` de la variante) |
| `Finie(Motif)` / `Coupee(Cause)` | terminal : l'état est retiré et l'orchestrateur agit | le `motif_fin_de_piste` (5 motifs de `decisions::motif_fin`) ou la cause d'arrêt (3) — aujourd'hui deux `bool` locaux, `track_ended` et `force_stop` |

Restent hors de l'enum, dans `ZonePollState` : 21 `track_generation`
(identité), 9-12 (métriques), 8 `journal`, 38-39 `famine`, 33
`last_device_volume`, 17 `scrobbled_key`, 18 `ticks_since_db_save`, 20
`last_seek_seen`, 3-4 tenue, 28-29 dépassement. Soit 17 champs de zone, 22
dans la machine, et 3 des 5 booléens absorbés par des variantes (`gapless_sent`,
`gapless_advance_pending`, `wall_clock_end_fired`) ; `tenue_signalee` et
`depassement_duree_signale` restent des verrous de constat, hors machine.

Table des transitions proposée (témoin entre crochets, voir
`poller/temoins_de_transitions_ref9.rs`) :

| De | Événement | Vers | Aujourd'hui |
|---|---|---|---|
| — | génération changée / `insert` | `Neuve` | 349, 401, 443-462 [T1] |
| `Neuve` | premier échantillon plausible | `Lecture` | 1189-1199 (`stale_start_position`) |
| `Lecture` | fenêtre des 30 s, `prepare_gapless` → `Armed` | `Armee` | 2110-2170 [T2] |
| `Armee` | durée changée + position confirme / position remise à 0 | `Lecture` (piste suivante) | 1989-2039, 1285-1360 [T3] |
| `Armee` | armement expiré / file changée | `Lecture` (même piste, ré-arme) | 2048-2108 [T10] |
| `Armee` | `Stopped` dans la garde, assez joué | `AvancePendante` | 1506-1524 [T4] |
| `Lecture` / `Armee` | `Stopped`, Tune joue | `Arretee` | 1594-1595 |
| `Arretee` | `Playing` | `Lecture` / `Armee` | 1860 |
| `Arretee` × 5, fin naturelle, armé, sortie capable | — | `AvancePendante` | 1652-1672 [T7b] |
| `Arretee` × 5, fin naturelle, flux servi | — | `Finie(natural_end_after_stopped)` | 1694-1701 [T7] |
| `Arretee` × 30, pas de fin, octets **à sec** | — | `Coupee(playback_failure)` | 1794-1807 [T8] |
| `Arretee` × 30, octets consommés ou **inconnus** | — | `Arretee` (on attend) | 1763-1793 [T8] |
| `AvancePendante` | `Playing` | `Lecture` | 1899-1916 [T5] |
| `AvancePendante` | 4 tours de refroidissement + 2 bloqués | `Finie(gapless_advance_stuck)` | 1490-1493, 1526-1544 [T6] |
| `Lecture` | position > durée + 3 s × 3 (ou horloge murale) | `Finie(position_past_end)` | 2282-2305 [T9] |
| `Lecture` | `ended_naturally` plausible / DSD-DLNA au pic | `Finie(local_ended_naturally / dlna_dsd_reached_end)` | 1554-1590 (couvert par `fsm::tests`) |
| `Lecture` × 30 `Playing` sans progrès ni octets | — | `Coupee(playing_stall)` | 2393-2419 |
| tout état | sonde `Err` | `SondageEnEchec` | 650-664 [T11] |
| `SondageEnEchec` ≥ 2, DLNA, horloge écoulée | — | `Finie` (verrou, l'état survit) | 694-714 [T11] |
| `SondageEnEchec` | sonde `Ok` | état précédent | 629 |
| tout état | `now_playing.source == "radio"` | `Radio` | 514-526, 828 |
| `Radio` × 6 `Stopped` sans position | — | `Coupee(radio_giving_up)` | 1014-1024 |

Trois points pour l'arbitrage :

1. `Arretee` est-il un état ou un compteur dans `Lecture` / `Armee` ?
   Aujourd'hui `stopped_ticks` s'accumule **sous** l'armement (armé, garde
   expirée, `Stopped` × 5 → 1652). Un état `Arretee { depuis: Lecture | Armee }`
   le dit ; un compteur dans chaque variante le cache.
2. `gapless_sent = true` sur sortie exclusive (2147) : sous-variante
   `Armee::Renonce`, ou pas d'état du tout et un `bool` de zone
   « ne plus tenter » ? Le second sens du drapeau est ce qui oblige à re-sonder
   `supports_internal_gapless()` quatre fois par tour.
3. `wall_clock_end_fired` : un état `SondageEnEchec { fin_prononcee }` suffit
   si l'état peut être retiré depuis la branche `Err` — ce que l'emprunt
   interdit aujourd'hui (674). Sinon le verrou reste.

## Ce qui reste dans `tick` après une telle machine

Par tranche de lignes, ce qui n'est **pas** de l'état de lecture et resterait
en place (ou sortirait dans des fonctions à part, mais pas dans la machine) :

| Lignes | Contenu | ≈ |
|---|---|---:|
| 26-319 | zones **au repos** : recul de sonde, volume, reprise depuis l'appareil (#729), tenue du renderer | 295 |
| 346-396 | zone **sans périphérique** : métadonnées radio, confirmation / abandon navigateur | 50 |
| 465-501 | scrobble | 35 |
| 531-718 | **sondes** : panne de sortie, `try_lock`, `get_status`, latence, journal d'échec | 190 |
| 720-822 | **famine** de l'anneau (#3318, #3814) | 100 |
| 828-1034 | chemin **radio** (deviendrait `Radio`, mais le rafraîchissement des métadonnées et la relance restent des effets) | 205 |
| 1036-1171 | déplacement, **position publiée**, volume, sauvegarde en base | 135 |
| 1850-1857, 2429-2439 | ombre FSM (disparaît avec la machine) | 20 |
| 2447-2465 | **métriques** partagées | 20 |
| 2467-2543 | **effets** : `stop`, relance « démarrage mort », `track_end_gap`, `handle_track_end` | 75 |

Soit ≈ 1 125 lignes d'effets, de sondes et de journal ; la machine remplacerait
les ≈ 1 400 lignes restantes (1173-1372 position et remise à zéro, 1374-1848
bras `Stopped`, 1859-2427 bras `Playing`), dont une bonne part est commentaire
et journal — 31 lignes `info!`/`warn!`/`debug!` dans ces deux bras (60 dans
`tick` entier), chacune attachée à une issue du classifieur. Les journaux suivraient les transitions,
pas l'inverse.

## Témoins

`tune-core/src/poller/temoins_de_transitions_ref9.rs`, onze témoins T1-T11,
un par ligne marquée `[Tn]` ci-dessus. Chacun construit `ZonePollState`, le
passe à `fsm::classify_stopped` / `classify_playing` ou au prédicat de
`decisions`, applique les écritures que `tick` fait sur cette issue (recopiées
avec leur ligne) et observe l'état après ; il vérifie en outre que l'écriture
recopiée figure bien dans `tick.rs` à quelques lignes du journal qui nomme la
branche, pour que le miroir ne survive pas à un `tick` qui change.

## Décisions du 12/09 (par défaut, arbitrage de Bertrand attendu)

Nuit du 12 au 13/09, agent E, sur `batch/bugs-12` à `c503d33a` (les lignes
de `tick.rs` citées plus haut valent +6 sur cette base). Aucun comportement
ne change : l'énumération est **en ombre**, écrite par `tick`, jamais lue par
lui. Les trois points ouverts sont tranchés par défaut, pour que le dessin
se discute sur du code qui tourne :

1. **« Arrêtée » est un état** : `Arretee { depuis: Depuis }`, avec
   `Depuis::Lecture | Depuis::Armee(Armement)`. L'arrêt retient d'où il
   vient, parce que l'armement survit à l'arrêt (armé, garde expirée,
   `Stopped` × 5 → attente d'enchaînement, ligne 1652).
2. **Le second sens de `gapless_sent` est une variante distincte** :
   `Armee { armement: Armement }` avec `Armement::Accepte { ligne }`
   (`SetNext` accepté, `ligne` = ce que `gapless_armed` porte) et
   `Armement::Renonce` (sortie exclusive, rien n'est parti, 2147). La
   cohérence exige `gapless_armed == None` et `gapless_sent_at == None`
   sous `Renonce`.
3. **Le retrait depuis la branche d'erreur passe par une transition
   nommée** : `FinParHorlogeMurale` fait passer `SondageEnEchec` à
   `Terminee(Finie(HorlogeMuraleSurSondeEnEchec))`. L'état survit à
   l'emprunt (674) ; c'est le seul état terminal qui survive à un tour, et
   le seul qui ait le droit de porter `wall_clock_end_fired = true`. Depuis
   `Terminee`, seules `SondeEnEchec`, `SondeRetablie` (identités) et
   `NouvellePiste` sont admises : un état terminal ne « joue » plus.

Une variante ne porte que ce qui la **distingue** ; les compteurs et
horloges que la proposition lui attribuait restent dans les 39 champs tant
que l'ombre ne pilote rien — les recopier ferait deux écrivains pour un même
fait, et c'est précisément ce que l'invariant doit rendre impossible.

### L'énumération (`poller/etat.rs`)

| Variante | Champs portés | Ce que les drapeaux doivent dire (`coherent()`) |
|---|---|---|
| `Neuve` | — | `gapless_sent=false`, `gapless_armed=None`, `gapless_sent_at=None`, `gapless_advance_pending=false`, `stopped_ticks=0` |
| `Lecture` | — | idem |
| `Armee { armement }` | `Accepte { ligne: Option<ArmedNext> }` ou `Renonce` | `gapless_sent=true`, `gapless_armed == ligne` (ou `None` + pas d'horodatage sous `Renonce`), `pending=false`, `stopped_ticks=0` |
| `Arretee { depuis }` | `Depuis::Lecture` ou `Depuis::Armee(armement)` | `stopped_ticks > 0`, `pending=false`, et les drapeaux de `depuis` |
| `AvancePendante` | — | `gapless_advance_pending=true`, désarmé, `stopped_ticks=0` |
| `Radio` | — | comme `Lecture` |
| `SondageEnEchec { precedent }` | l'état d'avant, restitué au prochain `Ok` | `consecutive_errors > 0`, et les drapeaux de `precedent` |
| `Terminee(Issue)` | `Finie(MotifFin)` (5 motifs de `motif_fin` + horloge murale) ou `Coupee(CauseDeCoupure)` (4 causes) | retiré dans le tour ; sauf horloge murale : `wall_clock_end_fired=true` |

Hors état terminal, `wall_clock_end_fired` doit être faux. `ZonePollState`
gagne le champ `etat: EtatDeLecture` **à côté** des 39 champs — aucun
retiré, aucun renommé. Les cinq littéraux `ZonePollState { … }` de
`poller/tests.rs` reçoivent la ligne `etat: EtatDeLecture::Neuve` (ils ne
passent jamais par `tick`).

### Les 22 transitions (`poller/fsm.rs`) et leurs sites dans `tick`

`fn transition(&mut self, t: Transition)` applique la table `suivant` —
**sans bras `_`** : chaque couple (état, transition) est écrit ; un état de
départ imprévu est une `Incoherence::TransitionInattendue` journalisée
(`poller_etat_transition_inattendue`) qui laisse `etat` tel quel. Sites
instrumentés sur `batch/bugs-12` après insertion (un appel ajouté juste
après l'écriture, aucune écriture ni condition modifiée, aucune ligne
déplacée) :

| # | Transition | De → vers | `tick.rs:` (après insertion) |
|--:|---|---|---|
| 1 | `NouvellePiste` | tout → `Neuve` | 466 |
| 2 | `PremierEchantillonPlausible` | `Neuve` → `Lecture` (identité ensuite) | 1212 |
| 3 | `Armement { armement }` | `Lecture` → `Armee` | 2196 (`Renonce`), 2217 (`Accepte`) |
| 4 | `TransitionDetectee` | `Armee` / `Arretee{Armee}` → `Lecture` | 1336, 2061 |
| 5 | `Desarmement` | `Armee` → `Lecture` | 2102, 2151 |
| 6 | `ArretDansLaGarde` | `Armee{Accepte}` → `AvancePendante` | 1543 |
| 7 | `RendererArrete` | `Lecture` / `Armee` → `Arretee{depuis}` | 1622 |
| 8 | `ArretEfface` | `Arretee{depuis}` → `depuis` | 1411, 1491, 1500, 1511, 1900, 2493 |
| 9 | `FinNaturelleEnAttenteDEnchainement` | `Arretee{Armee}` → `AvancePendante` | 1700 |
| 10 | `FinNaturelleApresArret` | `Arretee` → `Terminee(Finie)` | 1732 |
| 11 | `PanneDeLecture { cause }` | `Arretee` → `Terminee(Coupee)` | 1762 (`RendererCale`), 1844 (`FluxASec`) |
| 12 | `AttenteProlongee` | `Arretee` → `Arretee` | 1799, 1818 |
| 13 | `EnchainementConfirme` | `AvancePendante` → `Lecture` | 1942 |
| 14 | `EnchainementBloque` | `AvancePendante` → `Terminee(Finie)` | 1564 |
| 15 | `PositionAuDelaDeLaFin` | `Lecture` / `Armee` → `Terminee(Finie)` | 2353 |
| 16 | `FinConstateeAvantLeSeuil { motif }` | `Lecture` / `Armee` / `Arretee` → `Terminee(Finie)` | 1593, 1614 |
| 17 | `LectureSansProgres` | `Lecture` / `Armee` → `Terminee(Coupee)` | 2468 |
| 18 | `SondeEnEchec` | tout → `SondageEnEchec{precedent}` | 662 |
| 19 | `FinParHorlogeMurale` | `SondageEnEchec` → `Terminee(Finie)` | 722 |
| 20 | `SondeRetablie` | `SondageEnEchec{p}` → `p` (identité ailleurs) | 634 |
| 21 | `SourceRadio` | `Neuve` / `Radio` → `Radio` | 832 |
| 22 | `RadioAbandonnee` | `Radio` → `Terminee(Coupee)` | 1031 |

**22 transitions sur 22 instrumentées, 33 appels.** Trois remarques :

- La naissance (`or_insert_with(ZonePollState::new)`, 350 et 404) n'est pas
  un appel : `new` construit `Neuve`.
- `ArretEfface` couvre six sites de `stopped_ticks = 0` (renderer qui joue,
  pause, Tune qui ne joue plus, trois grâces) : c'est le même geste, et la
  table du document ne nommait que le premier (1860).
- Les transitions **non témoignables sans sonde** (2165, 2147, 183, 1697,
  712 — voir plus haut) sont instrumentées quand même : l'appel ne dépend
  pas de la sonde, seulement de l'écriture qu'il suit. Aucune n'est « non
  atteignable » pour l'ombre. `tenue_signalee` et
  `depassement_duree_signale` restent hors machine, comme proposé.

### L'invariant

`fn coherent(&self) -> Result<(), Incoherence>` dans `etat.rs`, table
ci-dessus ; `Incoherence::Drapeau` nomme l'état, le drapeau, l'attendu et
le lu. Vérifié à **un seul site**, à la fin de `tick`, sous
`cfg(debug_assertions)` : pour chaque zone encore sondée,
`debug_assert!(verdict.is_ok(), "poller_etat_incoherent zone_id=… : …")`.
Aucune décision de `tick` ne lit `etat` ; en release rien n'est vérifié,
seules les transitions inattendues sont journalisées.

### Témoins (`poller/temoins_de_transitions_ref9.rs`)

T1-T11 inchangés. E0 (la table n'a pas de bras muet ; l'invariant nomme
l'état et le drapeau ; le site unique du `debug_assert!`), E1-E22 (un par
transition : état construit, décision de `fsm`/`decisions`, miroir des
écritures, **l'incohérence que l'invariant rendrait sans l'appel**, la
transition, `coherent()`, et l'ancrage de l'appel `ps.transition(…)` au
texte de `tick.rs`), E23 (le `tick` de production sur un renderer factice :
`Neuve` → `Lecture` → `Arretee` → `Lecture`, le `debug_assert!` de fin de
tour traversé à chaque tick). Le banc `lire_ensuite_dans_la_fenetre_gapless`
traverse le même `debug_assert!` sur l'armement et la transition détectée.

Contre-épreuve : retirer l'appel `TransitionDetectee` du site
`gapless_position_reset_detected` (1336) fait rougir E4 (« sans cet appel,
l'invariant rend : etat=Arretee drapeau=gapless_sent attendu=true
lu=false ») **et** le banc #3026 (`poller_etat_incoherent … etat=Armee
drapeau=gapless_sent attendu=true lu=false`, dans `tick` lui-même) ; la
restauration par `cp` rend les deux verts. Sorties collées dans la PR.

### Ce qui attend l'arbitrage

Rien de `tick` ne lit `etat`. Basculer une lecture (par exemple
`if ps.gapless_sent` → `matches!(ps.etat, Armee { .. })`), retirer un
drapeau absorbé (`gapless_sent`, `gapless_advance_pending`,
`wall_clock_end_fired`), ou déplacer un compteur dans sa variante : chacun
de ces gestes change une décision et attend le dessin arrêté. La
proposition ci-dessus reste une proposition ; l'ombre ne fait que la
mesurer.
