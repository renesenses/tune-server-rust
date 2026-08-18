# Cahier de recette — Tune v0.9.0 (Release Candidate)

**Version** : v0.9.0-rc1 · **Canal** : beta (testeurs qualifiés)
**Testeurs** : DEvir (Séoul), Silviu (Roumanie), … — à compléter
**Plateformes** : macOS (DMG), Windows (zip/setup), Linux (tar.gz / Docker), iPad (TestFlight)

> **v0.9.0 = train de stabilisation.** Priorité aux **non-régressions** : la refonte
> interne (poller FSM, consolidation de la file d'attente) ne doit RIEN changer au
> comportement. Ce cahier est un **stub** — la QA l'étoffe au fil des RC.

## Instructions

Ouvrir le client web : `http://<adresse-serveur>:8888`. Pour chaque test, noter
**OK** / **KO** + un commentaire si KO (comportement observé, zone, sortie audio,
version exacte via `/api/v1/system/version`, logs si possible).

---

## 1. Smoke test — démarrage & version

| # | Test | Résultat |
|---|------|----------|
| 1.1 | Le serveur démarre, `/api/v1/system/version` renvoie `0.9.0-rc1` | |
| 1.2 | Le client web charge (dashboard, bibliothèque, zones) | |
| 1.3 | Scan de bibliothèque : pistes/albums/artistes cohérents | |

## 2. Lecture & gapless (zone la plus à risque — poller FSM)

| # | Test | Résultat |
|---|------|----------|
| 2.1 | Lecture locale d'un album complet **sans coupure** entre pistes (gapless) | |
| 2.2 | **Repeat one** sur sortie ASIO / WASAPI exclusive → la piste reboucle (fix DEvir) | |
| 2.3 | Repeat all / album → enchaînement correct, métadonnées « Now Playing » justes | |
| 2.4 | Seek en cours de lecture → pas d'arrêt, métadonnées conservées | |
| 2.5 | Fin de piste courte (< 30 s) reconnue, enchaînement OK | |
| 2.6 | Lecture DLNA (DMP-A8/A10, Bluesound…) : transition gapless, pas de coupure à 30 s | |

## 3. File d'attente (consolidation `queue_items`)

| # | Test | Résultat |
|---|------|----------|
| 3.1 | Ajouter des pistes locales à la file → ordre correct, lecture OK | |
| 3.2 | Ajouter un titre streaming (Qobuz/Tidal) pendant une lecture locale → il apparaît et se joue (pas invisible) | |
| 3.3 | Vider / réordonner la file → cohérent après redémarrage serveur | |

## 4. Zones (stabilité)

| # | Test | Résultat |
|---|------|----------|
| 4.1 | Créer / supprimer une zone → une zone supprimée **ne réapparaît pas** après re-découverte | |
| 4.2 | Comptage de zones exclut les zones masquées | |
| 4.3 | Multi-zones simultanées : lecture indépendante | |

## 5. Streaming & qualité

| # | Test | Résultat |
|---|------|----------|
| 5.1 | Badge qualité correct (format / sr / bd) sur flux local et streaming | |
| 5.2 | Tidal 24/192 (DASH) sur sortie locale / ASIO : lecture correcte | |
| 5.3 | Qobuz lossless : badge et lecture corrects | |

## 6. Divers

| # | Test | Résultat |
|---|------|----------|
| 6.1 | Radios : import d'une playlist `.pls` → stations importées | |
| 6.2 | Crossfade par zone : réglage persistant après redémarrage | |
| 6.3 | Secrets absents de `GET /system/config` (sauf `?include_secrets=true`) | |

---

## Régressions bloquantes (P0)

Signaler immédiatement, avec logs, tout : coupure gapless, zone fantôme, crash,
lecture qui s'arrête seule, badge qualité faux, perte de la file au redémarrage.

*Cahier stub — à enrichir par la QA au fil des RC (rc.2 → rc.4 → GA).*
