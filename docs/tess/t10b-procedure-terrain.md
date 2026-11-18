# T10b — Procédure de mesure terrain (correction de pièce par FIR mesuré)

> Statut : **préparée, pas encore exécutée.** Tant que deux pièces réelles
> n'ont pas été mesurées avec cette procédure, aucune revendication
> « correction de pièce intégrée » (décision **Q10**) : ni sur le site, ni
> dans le comparatif Roon, ni dans les notes de version.
>
> Pièce 1 : **celle de Bertrand** (il a un micro de mesure). Pièce 2 : un
> testeur, à trouver (semaine du 30/11 visée par le plan).

La preuve hors ligne (simulation) est dans
`tune-calibration/tests/fir_simule.rs`. Ici, on vérifie la même chose dans
une vraie pièce : la réponse corrigée, remesurée à plusieurs places, tient la
cible à ±1,5 dB de 30 Hz à 15 kHz (lissage variable), et l'écoute A/B ne
dégrade rien.

## 1. Matériel

- Un micro de mesure **USB** (miniDSP UMIK-1 ou UMIK-2, ou équivalent). Pas
  le micro d'un téléphone, pas un micro de voix.
- Son **fichier d'étalonnage** s'il existe : miniDSP le fournit par numéro de
  série (fichier `.txt`, prendre la version **90°** si le micro pointe vers
  le plafond, **0°** s'il pointe vers les enceintes). Un `.cal` REW convient
  aussi.
- Un pied de micro (ou une pile de livres stable), à hauteur d'oreille assis.
- Le serveur Tune de la pièce, avec :
  - le greffon **calibration** installé (il est opt-in) ; vérification :
    `GET /api/v1/ext/calibration/microphones` répond 200 et liste le micro ;
  - un compte **Premium** (la correction mesurée est Premium, décision D1) ;
  - la zone qui sort sur les enceintes de la pièce (noter son numéro).
- Un mètre ruban ; un téléphone pour deux photos de la pièce.

Variables utilisées ci-dessous :

```sh
TUNE=http://<adresse-du-serveur>:8888
CAL=$TUNE/api/v1/ext/calibration
ZONE=<numéro de la zone>
# Si l'API demande un jeton : ajouter  -H "Authorization: Bearer $JETON"
```

## 2. Préparer la pièce

1. Enceintes et place d'écoute **comme d'habitude** (on corrige l'écoute
   réelle, pas une installation de mesure).
2. Couper toute correction déjà active sur la zone : égaliseur, profil de
   correction de pièce, convolution (`POST $TUNE/api/v1/room-correction/ir/clear/$ZONE`).
   Le greffon refuse une mesure à travers des traitements actifs, mais
   vérifier quand même.
3. Volume : celui d'une écoute normale, plutôt bas. La mesure monte son niveau
   par salves (départ −30 dBFS, jamais au-dessus de −6 dBFS) et s'arrête dès
   que le rapport signal/bruit suffit. **Personne ne parle** pendant les
   balayages ; fenêtres fermées, chauffage soufflant éteint.
4. Mesurer la pièce (L × l × h) et noter le modèle des enceintes.

## 3. Déposer l'étalonnage du micro

Dans l'interface : **Correction de pièce → Au micro → Étalonnage**, déposer le
fichier. Ou :

```sh
curl -s -X POST $CAL/microphones/etalonnages -H 'content-type: application/json' \
  -d "$(jq -n --arg nom "UMIK-1 90°" --rawfile texte ./7xxxxxx_90deg.txt '{nom:$nom, texte:$texte}')"
```

## 4. Mesurer à plusieurs positions (session, 5 positions conseillées)

L'écran multi-positions (W6) n'est pas encore livré : la session passe par
l'API. Une mesure en un point reste possible dans l'interface, mais elle ne
sert pas à cette épreuve.

Positions (micro à hauteur d'oreille, toujours orienté pareil) :

| index | où |
|---|---|
| 0 | **place d'écoute** (tête), la centrale, poids 2 |
| 1 | 30 cm à gauche de la tête |
| 2 | 30 cm à droite |
| 3 | 30 cm en avant |
| 4 | 30 cm en arrière |

(3 à 9 positions acceptées ; au-delà de 5, ajouter les diagonales à 45 cm.)

```sh
# Créer la session
curl -s -X POST $CAL/zones/$ZONE/sessions -H 'content-type: application/json' \
  -d '{"positions":5,"centrale":0}'            # → {"id":"s…"}
S=<id rendu>

# Pour CHAQUE position : placer le micro, puis
curl -s -X POST $CAL/sessions/$S/positions -H 'content-type: application/json' \
  -d '{"entree":"<nom du micro tel que listé>","etiquette":"tête"}'
# attendre la fin (≈ 10–20 s) : GET $CAL/sessions/$S  → l'état de chaque position
```

Une position aberrante (micro heurté, bruit) est signalée « Refaire » dans
`GET $CAL/sessions/$S` : la refaire en nommant son `index`.

Sauvegarder la session **avant** correction :

```sh
curl -s $CAL/sessions/$S/export > avant-session-v2.json
```

## 5. Calculer le FIR et le charger en convolution

```sh
# Rapport : courbe de correction, latence, bornes qui ont agi
curl -s -X POST $CAL/sessions/$S/fir -H 'content-type: application/json' \
  -d '{"cible":{"type":"plate"}}' > fir-rapport.json

# Le filtre lui-même (à la fréquence de lecture habituelle, 48000 par défaut)
curl -s -X POST $CAL/sessions/$S/fir.wav -H 'content-type: application/json' \
  -d '{"cible":{"type":"plate"},"frequence":48000}' -o correction.wav

# Le charger en convolution sur la zone
curl -s -X POST --data-binary @correction.wav $TUNE/api/v1/room-correction/ir/upload/$ZONE
```

Bornes par défaut (prudentes) : coupe ≤ 15 dB, remontée ≤ 4 dB, **aucune
remontée dans un creux de 8 dB ou plus** (une annulation ne se comble pas),
correction de 25 Hz à 16 kHz, filtre normalisé à 0 dB (pas d'écrêtage).
Latence ajoutée : ≈ 43 ms (partie à phase linéaire). Lire les
`avertissements` du rapport et les recopier dans le retour.

Cible : commencer par `plate`. Si l'écoute paraît maigre, refaire avec
`{"type":"harman"}` ou `{"type":"plate","pente_db_oct":-0.8}` — **une seule
cible par passage**, notée dans le retour.

## 6. Remesurer, correction active

Le rapport du § 5 donne déjà la **réponse prévue** (`reponse_prevue_db`) et
son pire écart à la cible (`ecart_cible_prevu`). La remesure vérifie que la
convolution applique bien ce filtre dans la vraie chaîne (fréquence,
canaux, rééchantillonnage).

⚠️ La mesure du greffon **contourne** les traitements de la zone, convolution
comprise (#8513, avertissement « correction de pièce (IR) de la zone
contournée ») : une session du greffon remesure la pièce NUE. Pour la
remesure corrigée, deux voies :

- **REW, lecture de fichier** (pièce de Bertrand) : REW → *Generator* →
  exporter un balayage de mesure (`.wav`, 48 kHz, 256k) ; le lire **par
  Tune** sur la zone, convolution chargée ; REW en *Measure → File
  playback* capte avec le même micro et le même étalonnage, aux **mêmes 5
  places**. Exporter les 5 mesures (*File → Export → All measurements as
  text*) et la moyenne (*All SPL → Average the responses*, RMS).
- **Contrôle sans REW** (pièce testeur, à défaut) : une session du greffon
  après chargement doit redonner, aux erreurs de mesure près, la même
  moyenne qu'avant (preuve que la mesure entend la pièce nue) ; le verdict
  se fait alors sur la réponse prévue seulement, et le retour le dit.

```sh
# Contrôle : la session « après » (pièce nue) comparée à « avant »
curl -s -X POST $CAL/zones/$ZONE/sessions -H 'content-type: application/json' \
  -d '{"positions":5,"centrale":0}'            # → S2, mesurer les 5 places
curl -s $CAL/sessions/$S2/export > controle-session-v2.json
curl -s -X POST $CAL/comparaisons -H 'content-type: application/json' \
  -d "{\"avant\":\"$S\",\"apres\":\"$S2\",\"f_min_hz\":30,\"f_max_hz\":15000}" > comparaison.json
```

Manque connu, à trancher avant la pièce 2 : une option de mesure « à travers
la correction » dans le greffon rendrait la voie REW inutile.

## 7. Écoute A/B

- 3 morceaux connus (une voix, un morceau riche en basses, un acoustique).
- Basculer correction **on / off** (`ir/clear` puis `ir/upload`), si possible
  par une autre personne, sans dire lequel est lequel ; même niveau perçu
  (la normalisation baisse le niveau de quelques dB : compenser au volume).
- Noter pour chaque morceau : préférence (A / B / égal), et en une phrase :
  basses (traînantes ? plus nettes ?), voix, image stéréo, fatigue.

## 8. Ce qu'il faut nous renvoyer

| fichier / info | d'où |
|---|---|
| `avant-session-v2.json` | § 4 |
| `fir-rapport.json` et `correction.wav` | § 5 |
| mesures REW correction active (texte) et leur moyenne, ou `controle-session-v2.json` + `comparaison.json` | § 6 |
| fichier d'étalonnage du micro utilisé | § 3 |
| cible choisie, fréquence de lecture | § 5 |
| ressenti A/B (tableau du § 7) | § 7 |
| dimensions de la pièce, modèle d'enceintes, 2 photos (place d'écoute, enceintes) | § 2 |
| tout refus, avertissement ou comportement étrange (copie du JSON) | partout |

Envoi : fichiers déposés dans `~/DEV/coordination/tess/terrain-t10b/<pièce>/`
(pièce de Bertrand) ou transmis par Bertrand pour le testeur.

## 9. Verdict (rempli par nous, pas par le testeur)

Pour chaque pièce, à partir de la moyenne REW correction active (sinon de
`reponse_prevue_db`, et le verdict le dit) :

- écart à la cible, lissage variable (`fir::pire_ecart_a_la_cible`),
  30 Hz–15 kHz : pire point et sa fréquence ; attendu ≤ 1,5 dB **hors des
  creux non comblés** que le rapport annonce ;
- écart entre remesure REW et réponse prévue : attendu < 1 dB (sinon la
  chaîne de convolution n'applique pas le filtre prévu) ;
- A/B : aucune préférence nette pour « sans correction ».

Q10 n'est levée qu'avec **deux pièces** au verdict favorable, consigné dans
`decisions-tess.md` par Bertrand.
