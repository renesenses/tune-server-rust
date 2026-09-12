# Marge, écrêtage, crête vraie et dither — T9 de #2218

**État : mesuré, par des témoins qui rougissent.** Chaque chiffre de ce
document est produit par un test de `tune-core/tests/marge_et_crete_2218.rs`,
qui l'affirme dans son message d'assertion. Le fichier tourne sur **toute PR
Rust** (cible `[[test]]` sans caractéristique requise, job `Test` de `ci.yml`).
Aucun fichier de production n'a été modifié dans cette tranche : là où un
témoin révèle un défaut, il est nommé ici et dans un témoin `#[ignore]` qui
affirme le comportement attendu, à dé-ignorer par le correctif.

## Les quatre questions, et la réponse en une ligne

| # | question | réponse mesurée |
|---|---|---|
| 1 | le gain logiciel peut-il porter un échantillon au-delà de 0 dBFS ? | **oui** — ReplayGain sans pic tagué : +6 dB sur un sinus à −0,1 dBFS, **66,2 % des échantillons écrêtés dur**, excès max 31 866 LSB, sans compteur ; égaliseur passe-bas Q = 4 : **83,7 % d'overs**, comptés (`eq_overs`), écrêtés dur, jamais journalisés |
| 2 | ReplayGain + égaliseur produisent-ils des inter-échantillons > 0 dBFS ? | **oui quand le pic tagué est un pic d'échantillon** : carré à −0,05 dBFS posé au rail par `prevent_clipping`, crête vraie **+2,10 dBTP** ; avec le pic VRAI tagué : **−0,000 dBTP** (le mécanisme est juste, c'est le tag qui manque) |
| 3 | flottant → entier : dither, troncature ou arrondi ? | **quatre réponses différentes selon l'étage** : égaliseur = TPDF ±1 LSB + arrondi (16/24/32 bits) ; ReplayGain et mixeur = **troncature vers zéro** sans dither ; convolveur = arrondi sans dither (IR unité ≠ identité, −0,00027 dB) ; réduction 24→16 bits = **décalage** (troncature vers −∞) sans dither |
| 4 | PURE / désarmé : identité octet pour octet ? | **oui pour chaque étage désarmé** atteignable par une porte publique, à 16/24/32 bits ; la garde PURE de la sortie locale est privée (prouvée par T8, pas ici) |

## Protocole

* Signaux synthétiques déterministes, 44,1 kHz, une seconde (44 100
  échantillons) : sinus 997 Hz à −0,1 dBFS (et −20 dBFS pour la
  quantification), carré numérique 997 Hz et 50 Hz à −0,05 dBFS, impulsion à
  −0,1 dBFS, sinus à fs/4 déphasé de π/4 (contre-épreuve du mètre).
* Quantification de synthèse : arrondi au plus proche, saturé — c'est
  l'entrée, jamais l'objet de la mesure.
* Étages, par leurs portes **publiques** et dans l'ordre du bras progressif
  (`orchestrator.rs`, `PorteurDsp::process`) : `replaygain::gain_factor` +
  `apply_gain_pcm`, `eq::EqProcessor::{process_pcm, process_interleaved}`,
  `convolver::Convolver::process_pcm`, `crossfeed::CrossfeedProcessor`,
  `mixer::PcmMixer::apply_gain`, `decode::convert_pcm_bytes`.
* Écrêtage : compté contre la valeur **idéale** (entrée × facteur, avant
  saturation) — nombre d'échantillons dont l'idéal aurait arrondi au-delà du
  rail, et excès maximal en LSB. « Au rail » compte séparément les
  échantillons posés sur ±pleine échelle.
* Crête vraie : suréchantillonnage **×4** par interpolation sinc à fenêtre de
  Blackman-Harris 4 termes, 65 coefficients, écrite dans le test (≈ 30
  lignes), aucune dépendance nouvelle. Contre-épreuve : un sinus à fs/4
  déphasé de π/4 n'a aucun échantillon à sa crête (0,707·A) ; le mètre
  retrouve A à 0,3 % près (mesuré : 0,9000 pour A = 0,9). Les 32 premiers et
  derniers points ne sont pas interpolés : la troncature nette du tampon y
  fabrique un dépassement de bord (+1 % mesuré avant cette garde) qui n'est
  pas une crête du signal.
* Classification flottant → entier : erreur `sortie − idéal` en LSB, contre
  le signe de l'idéal. |e| ≤ 0,5 partout ⇒ arrondi ; e toujours du signe
  opposé à l'idéal ⇒ troncature vers zéro ; e toujours ≤ 0 ⇒ décalage
  (troncature vers −∞) ; sinon ⇒ bruit ajouté avant arrondi (dither).

## Ce qui a été mesuré

### Q1 — gain, écrêtage, nommage

| étage | stimulus | mesure | où ça sature | nommé ? |
|---|---|---|---|---|
| `gain_factor` + `apply_gain_pcm`, +6 dB, **pic non tagué**, `prevent_clipping` armé | sinus 997 Hz −0,1 dBFS, 16 bits | facteur ×1,9953 (rien ne le retient : seul le clamp ×4 de `gain_factor` borne) ; idéal +5,90 dBFS ; **29 174 / 44 100 écrêtés (66,2 %)**, excès max 31 866 LSB, 29 174 au rail ; signe conservé (saturation, pas d'enroulement) | `apply_gain_pcm`, clamp puis `as i16` | **non** : la fonction rend `()`, ni compteur ni journal |
| `EqProcessor::process_pcm`, bande `low_pass` 997 Hz Q = 4 | sinus 997 Hz −0,1 dBFS, 24 bits | `automatic_headroom_db` = **0 dB** (rien pour un filtre « pass ») ; résonance |H(fc)| = Q = +12,04 dB ; **36 896 / 44 100 overs (83,7 %)** ; au rail 32 412, au rail à 1 LSB près 36 896 | `write_sample_f64` : clamp à 1,0 − 1 LSB, **puis** dither ±1 LSB, puis arrondi — le plateau écrêté sort au rail ou 1 LSB dessous | compté (`EqProcessStats.overs`, exposé `eq_overs` dans `dsp_metrics` / signal-path), **jamais journalisé** |
| `EqProcessor::process_interleaved` (chemin flottant de la sortie locale), même profil | idem, f32 | 36 896 overs, crête **×3,954 (+11,94 dBFS)**, aucune saturation | plus loin : `f32_to_native_i32` (privé, WASAPI : arrondi + clamp) ou **personne** sur le chemin cpal flottant (macOS/Linux) | compté, pas journalisé |
| `EqProcessor::process_pcm`, `low_shelf` 80 Hz +6 dB (réserve −6 dB) | carré 50 Hz −0,05 dBFS, 24 bits | **17 825 overs (40,4 %)** ; crête flottante +0,45 dBFS : la réserve, somme des gains en dB (maximum **fréquentiel**), est courte de **0,50 dB** face à la réponse en **temps** d'un plateau d'ordre 2 | idem | idem |
| volume utilisateur | — | pas de témoin ici : `volume_scale` plafonne à l'unité (`le_plafond_est_l_unite`, `un_db_positif_est_refuse_pas_rabote`) | — | — |

### Q2 — crête vraie

| cas | crête d'échantillon | crête vraie ×4 |
|---|---|---|
| carré numérique 997 Hz brut | −0,05 dBFS | **+2,05 dBTP** — la reconstruction à bande limitée d'un échelon échantillonné dépasse de 13,7 % du saut (Σ sinc(½ − n) = 1,137), et le saut d'un carré ±A vaut 2A |
| même carré, ReplayGain +6 dB, **pic d'échantillon** tagué (0,9943), `prevent_clipping`, plafond 0 dBTP | facteur ramené à ×1,00577 = 1/pic ; le plateau positif est posé **1 LSB au-delà du rail** (22 050 échantillons, excès 0,99 LSB — le plafond 0 dB vise 1,0 = 2^23, non représentable) | **+2,10 dBTP** |
| idem, plafond −1 dBTP (#1694) | ×0,89640 | **+1,10 dBTP** — le plafond retire 1 dB, il ne mesure rien |
| idem, **pic VRAI** tagué (1,2658, ce que `rg_track_true_peak` contient quand l'analyse a tourné) | ×0,79004 (−2,05 dBFS) | **−0,000 dBTP** |
| chaîne complète du bras progressif : ReplayGain +6 dB (pic d'échantillon) **puis** égaliseur `peak` 3 kHz +6 dB Q 1 (réserve −6 dB), sinus 997 Hz −0,1 dBFS, 16 bits | après RG : 32 767 / −32 768 (au rail), après EQ : −4,72 dBFS, 0 over | après RG : **+0,000 dBTP** ; après EQ : −4,72 dBTP |

### Q3 — flottant → entier, étage par étage

| étage | 16 bits | 24 bits | 32 bits | détail |
|---|---|---|---|---|
| `EqProcessor::process_pcm` | dither | dither | dither (silence ressort à ±1 LSB) | TPDF ±1 LSB puis arrondi ; erreur moyenne +0,001 / +0,004 LSB, max 1,47 / 1,46 LSB — référence : les mêmes mots élargis à 32 bits par la même cascade |
| `replaygain::apply_gain_pcm` (−1 dB) | troncature vers zéro | troncature vers zéro | troncature vers zéro | `clamp` puis `as i16` / `as i32` ; **un facteur de 1 − 10⁻⁷ (−0,000001 dB) déplace 44 098 / 44 098 échantillons non nuls d'1 LSB vers zéro** |
| `PcmMixer::apply_gain` (−1 dB) | troncature vers zéro | troncature vers zéro | troncature vers zéro | `SampleFormat::write`, même écriture |
| `Convolver::process_pcm`, IR unité | arrondi | — | — | décode ÷32 768, encode ×32 767 : **29 214 / 44 100 échantillons (66 %) perdent 1 LSB** — une IR unité n'est pas l'identité (−0,00027 dB) |
| `decode::convert_pcm_bytes` 24 → 16 | décalage (troncature vers −∞) | — | — | 385 → 1 (un arrondi donnerait 2), −1 → −1, 255 → 0 ; chemin du transcodage 16 bits (DLNA/WAV) d'une source 24 bits et de la mémoire de préchargement |
| `outputs::local::f32_to_native_i32` | *non témoignable* | | | privé (`local.rs`), lu : `.round().clamp()`, sans dither |
| `decode::StreamingPcmByteAdapter::resample` | *non témoignable* | | | `pub(crate)`, lu : `clamp` puis `as i32` — troncature vers zéro après le SRC |

### Q4 — identité des étages désarmés (16, 24, 32 bits)

Vérifié octet pour octet : ReplayGain `Off` (facteur exactement 1,0, retour
immédiat), `EqProcessor` désactivé, `EqProcessor` armé à bandes neutres
(`is_enabled() == false`), `PcmMixer::apply_gain(1.0)` (non court-circuité,
×1 exact), chemin flottant désarmé (égaliseur off + crossfeed à 0).

## Ce qui est prouvé, ce qui ne l'est pas

**Prouvé** (14 témoins verts, sur toute PR Rust) : les comportements du
tableau ci-dessus, tels qu'ils sont. Les 5 témoins `#[ignore]` sont
**rouges** quand on les lance (`cargo test … -- --ignored`) : ce sont des
défauts, pas des intentions.

**Non prouvé ici** :

* la garde PURE de la sortie locale (`local_dsp_is_identity`,
  `pcm_bytes_to_native_i32`) et la conversion `f32_to_native_i32` — privées
  à `outputs/local.rs`, derrière `local-audio`. L'identité décodeur → puits
  est tenue par T8 (`capture_bout_en_bout_2218.rs`, sous `ci:full`) ;
* ce que fait le **pilote** d'un échantillon flottant > 1,0 sur le chemin
  cpal (macOS/Linux) : Tune ne sature pas, le résultat dépend de CoreAudio /
  ALSA. Essai matériel, hors de portée d'une machine de compilation ;
* la crête vraie du **mètre de l'analyse** (`analyzer.rs`, Catmull-Rom) n'est
  pas comparée ici au mètre ×4 du test ; BS.1770-5 prescrit un FIR ×4, ni
  l'un ni l'autre n'en est l'implémentation normative.

## Cases de #2218

* **Cochable** : « Headroom, clipping, true peak et dithering testés » — les
  quatre sont mesurés par des témoins qui rougissent, avec les défauts nommés.
* **Pas encore** : « Méthodes de mesure alignées avec AES17 ; true peak ITU-R
  BS.1770-5 » — le mètre ×4 de ce banc est du type prescrit par BS.1770-5
  mais n'en reprend pas le FIR normatif, et le mètre de production
  (`analyzer.rs`) est une interpolation de Catmull-Rom.

## Issues à ouvrir (à la main de Bertrand)

### A — ReplayGain : sans pic tagué, `prevent_clipping` n'empêche rien et `apply_gain_pcm` écrête dur sans compter

Mesuré : +6 dB sur un sinus à −0,1 dBFS, 66,2 % d'échantillons écrêtés,
excès 31 866 LSB, aucun compteur, aucun journal. Attendu : `prevent_clipping`
armé ⇒ aucun échantillon au-delà du rail, pic tagué ou non (refuser le gain
positif sans pic, ou l'analyser) ; et `apply_gain_pcm` compte ses écrêtés
comme `EqProcessStats.overs`, exposés dans `signal-path`. Témoin :
`q1_defaut_connu_prevent_clipping_arme_ne_devrait_jamais_ecreter_meme_sans_pic_tague`.

### B — Égaliseur : la réserve automatique ignore la résonance des passe-bas/haut et la réponse en temps des plateaux

Mesuré : `low_pass` Q = 4 ⇒ réserve 0 dB, résonance +12,04 dB, 83,7 % d'overs
écrêtés dur ; `low_shelf` +6 dB sur un carré ⇒ 40,4 % d'overs, réserve courte
de 0,50 dB. Attendu : réserver 20·log10(Q/0,707) pour un `low_pass` /
`high_pass` à Q > 0,707, et couvrir la norme L1 de la cascade (ou une marge
fixe documentée) pour les plateaux. Témoins :
`q1_defaut_connu_la_reserve_automatique_devrait_couvrir_la_resonance_d_un_passe_bas`,
`q1_defaut_connu_la_reserve_automatique_devrait_couvrir_la_reponse_en_temps_d_un_plateau`.

### C — ReplayGain : avec un pic d'échantillon tagué, `prevent_clipping` laisse passer les inter-échantillons

Mesuré : +2,10 dBTP sur un carré, +0,000 dBTP sur un sinus ; le plafond
−1 dBTP retire 1 dB sans mesurer ; avec `rg_track_true_peak` la crête tient
à 0 dBTP. Attendu : quand seul `rg_track_peak` existe (tags externes), le
dire dans `signal-path` et appliquer une marge par défaut, ou déclencher
l'analyse qui écrit le pic vrai. Témoin :
`q2_defaut_connu_prevent_clipping_devrait_tenir_la_crete_vraie_sous_0_dbtp_avec_un_pic_d_echantillon`.

### D — Réduction 24 → 16 bits par décalage, sans dither

Mesuré : `convert_pcm_bytes` tronque vers −∞ (385 → 1). Chemins : transcodage
16 bits (DLNA / WAV) d'une source 24 bits, mémoire de préchargement servie à
une sortie moins profonde. Attendu : TPDF avant arrondi, comme
`EqProcessor::write_sample_f64` le fait déjà. Témoin :
`q3_defaut_connu_la_reduction_24_vers_16_bits_devrait_dither`.

### E — `apply_gain_pcm` et `PcmMixer::apply_gain` tronquent vers zéro sans dither

Mesuré : un gain de −0,000001 dB déplace tout le signal d'1 LSB vers zéro ; à
−1 dB, erreur corrélée au signe du signal (distorsion de troncature, pas de
bruit). Attendu : arrondi au plus proche au minimum, TPDF de préférence — le
dither de l'égaliseur est déjà là, à partager. Constat voisin, sans témoin
ignoré : une IR unité du convolveur perd 1 LSB sur 66 % des échantillons
(×32 767 / 32 768). Pas de témoin `#[ignore]` : l'attendu est un choix de
conception (arrondi ou dither) à trancher dans l'issue.

## Contre-épreuves

Deux témoins sabotés par `sed` (copie `cp` avant), rouges nommés, restaurés
par `cp` + `touch`, verts — sorties collées dans la PR :

* `q1_replaygain_sans_pic_tague_…` : signal abaissé de −0,1 à −12,1 dBFS ⇒
  « écrêtage massif attendu (~66 %) : 0/44100 » ;
* `q3_convert_pcm_bytes_…` : vecteur attendu remplacé par celui qu'un arrondi
  donnerait ⇒ `left: [1, 1, 1, -2, -2, -1, 0, 0]` / `right: [2, 2, 1, -1, -2, 0, 0, 0]`.

## Reproduction locale

```sh
cargo test -p tune-core --test marge_et_crete_2218 -- --nocapture   # 14 verts, 5 ignorés
cargo test -p tune-core --test marge_et_crete_2218 -- --ignored      # 5 rouges : les défauts A–D
```

## Comptage livré le 12/09 (agent F, `tune-core/tests/ecretage_compte_2218.rs`)

**Aucun échantillon n'a bougé.** Les empreintes FNV-1a des octets de sortie
des étages sur les signaux de ce banc (ReplayGain +6 dB sans pic à 16/24/32
bits, −1 dB à 16/24/32 bits, ReplayGain avec pic tagué, égaliseur passe-bas
Q = 4 entier et flottant, plateau grave sur carré, chaîne ReplayGain puis
égaliseur, mixeur −1 dB et ×2 : 15 empreintes) ont été relevées sur
`batch/bugs-12` à 49ecf1fe **avant** le comptage par un témoin temporaire non
publié, collées dans `ecretage_compte_2218.rs`, et sont **inchangées après**.
Les 14 témoins verts et les 5 ignorés de ce banc n'ont pas été touchés.
Le clamp de chaque étage est resté où il est, dans l'ordre où il est (clamp
PUIS dither pour l'égaliseur), avec ses seuils.

### Ce qui est compté, et où

`tune-core/src/audio/ecretage.rs` : `CompteurDEcretage { echantillons_vus,
echantillons_ecretes, exces_max_lsb, crete_max, premier_ecretage_a }` — des
champs simples, zéro allocation, deux comparaisons par échantillon, une
addition par bloc. « Écrêté » = la condition du clamp de l'étage, ni plus ni
moins ; l'excès en LSB de la profondeur traitée (24 bits de référence pour le
chemin flottant de l'égaliseur, qui n'a pas de profondeur).

| étage | compteur | où il est incrémenté | par |
|---|---|---|---|
| ReplayGain | `apply_gain_pcm_compte(pcm, bits, facteur, &mut CompteurDEcretage)` ; `apply_gain_pcm` (signature inchangée) délègue et cumule dans le registre | `replaygain.rs`, juste avant le `clamp`, sur la valeur idéale | appel (bloc) ; **piste** avec `GainReplay` |
| égaliseur | `EqProcessor::ecretage()` — `echantillons_ecretes` vaut exactement `process_stats().overs` | `eq.rs`, la branche `stats.overs += 1` de `process_pcm` et `process_interleaved` | **piste** (le processeur est bâti par piste ; `inherit_state_from` relaie le compteur et ses « déjà dit ») |
| mixeur | registre seulement | `mixer.rs`, `PcmMixer::apply_gain`, avant `SampleFormat::write` ; `mix_into` (appelable d'un rappel temps réel) n'est pas touché | appel |

Cas A rejoué : 29 174 / 44 100 écrêtés (66,2 %, à 8 près de l'arrondi de ce
banc : le compteur suit la condition du clamp, `> 32 767` ou `< −32 768`),
excès max **31 866 LSB**, crête idéale +5,90 dBFS, premier écrêtage au 4ᵉ
échantillon. Cas B rejoué : 36 896 overs (83,7 %), crête +11,94 dBFS.

**Fil d'exécution, vérifié** : ces étages tournent côté producteur — relais
du bras progressif (`spawn_streaming_dsp_relay`, tâche tokio), transcodage
complet (`orchestrator/transcodage.rs`), et pour la sortie locale
`apply_local_dsp`, appelé par `process_pcm_chunk` / `prepare_windows_*_pcm` /
`play_url`, jamais par les rappels cpal (`build_output_stream`, qui ne font
que vider l'anneau). Le `warn!` est émis après un bloc, jamais dans la boucle.

### Ce qui est dit : deux lignes `dsp_ecretage` par piste

Niveau WARN, champs `etage` (`replaygain` / `egaliseur`), `moment`
(`premier` / `fin`), `portee` (`piste` / `processus`), `echantillons_vus`,
`echantillons_ecretes`, `pourcentage`, `exces_max_lsb`, `crete_max_dbfs`,
`premier_ecretage_a`. La première après le PREMIER bloc qui écrête, la seconde
à la destruction du porteur avec le total ; rien pour une piste propre ; jamais
une ligne par bloc (témoin : 100 blocs écrêtants ⇒ 4 lignes pour deux étages,
pas 200). La **zone** n'est pas connue de ces étages (un facteur, un profil) :
elle vient du `Span` de l'appelant quand il en tient un.

Limite, dite : sur le bras progressif, `StreamingDsp.replaygain` est un
`Option<f64>` nu (`orchestrator.rs`) — `apply_gain_pcm` ne connaît pas la
piste. Il compte dans le registre à chaque bloc et ne dit qu'UNE ligne
`portee=processus`, au premier bloc du processus qui écrête. Les deux lignes
par piste du ReplayGain sont portées par `GainReplay` (`process` + `Drop`),
témoigné ici, que l'orchestrateur ne porte pas encore (hors périmètre de la
nuit : `orchestrator/` a d'autres écrivains). Pour l'égaliseur, les deux lignes
par piste sont livrées sur tous les chemins, sans branchement à faire.

### Où c'est lu

Rapport de diagnostic (`routes/system/diagnostics.rs`), JSON et Markdown :
section `dsp_ecretage` — par étage, `echantillons_vus`, `echantillons_ecretes`,
`pourcentage`, `exces_max_lsb`, `appels_ecretants`, `pistes_ecretees`,
`lignes_journal`, depuis le démarrage du processus, tous flux confondus
(`audio::ecretage::REGISTRE`, `AtomicU64`). Pas par zone : `OutputDspMetrics`
est construit par littéral dans `outputs/local.rs`, hors périmètre.

### Les cinq défauts, toujours à trancher par Bertrand

Rien de ce qui suit n'a été corrigé ; c'est maintenant compté et dit.

* **A** — `prevent_clipping` sans pic tagué n'empêche rien (66 % écrêtés) ;
* **B** — la réserve automatique ignore la résonance des passe-bas/haut et la
  réponse en temps des plateaux (83,7 % / 40,4 % d'overs) ;
* **C** — avec un pic d'échantillon tagué, la crête vraie passe à +2,10 dBTP ;
* **D** — réduction 24 → 16 bits par décalage, sans dither ;
* **E** — `apply_gain_pcm` et `PcmMixer::apply_gain` tronquent vers zéro sans
  dither (le comptage garde le même `clamp` puis le même `as`).

```sh
cargo test -p tune-core --test ecretage_compte_2218 -- --nocapture   # 8 verts
```
