# Mesurer avant de toucher — le noyau temps réel de Tune OS (#3205)

**État : protocole, pas résultat.** Ce fichier dit *ce qu'on mesure*, *comment*,
et *ce qu'on en conclut*. Il ne contient aucun chiffre de terrain : aucun n'a
été relevé à ce jour.

## Ce que la mesure décide

Le noyau `PREEMPT_RT` de Tune OS coûte **le Secure Boot** et un **COPR non
signé**, à tous ses utilisateurs. L'arbitrage de Jean-Philippe Robbe
(02/09/2026) :

> Si les xruns sont à zéro sur noyau standard, le noyau RT est un coût sans gain
> et le Secure Boot revient.

Ce ticket peut donc conclure à **retirer** du travail. C'est son intérêt
principal, et c'est aussi ce qui le rend dangereux : un zéro obtenu par erreur
conclut dans le sens du retrait, sans que personne ait l'occasion de s'en
apercevoir.

## Le chiffre qu'on mesure, et pourquoi celui-là

**Pas la latence d'ordonnancement.** Arbitrage du 02/09, mot pour mot :

> Cyclictest mesure la latence d'ordonnancement, mais avec un ring de deux
> secondes et une garde de 500 ms, cette latence n'est pas ce qui fait sauter
> l'audio. Ce qui compte, c'est **combien de fois le callback a manqué de
> données**. […] Cyclictest vient après, seulement si le compteur n'est pas à
> zéro.

**La famine de l'anneau**, donc : le nombre de fois où le rappel du pilote a
réclamé de l'audio et reçu des zéros. Livré par la PR #3219 dans le tag
`v0.9.131`.

### Pourquoi ce chiffre est fiable

1. **Il est compté au seul endroit que tous les backends partagent.** Il y a
   sept sites de famine — les quatre chemins cpal, CoreAudio exclusif, sept
   rappels ASIO, WASAPI. Le comptage est logé **dans le drain de l'anneau**,
   l'unique objet que les sept traversent. Aucun backend ne change de
   signature, et un backend futur ne peut pas oublier de compter.
2. **Il porte son propre dénominateur.** `served_samples` et `stream_ms`
   partent avec lui. « 3 événements » ne se compare à rien ; « 3 événements sur
   352 s de flux » se compare d'une machine à l'autre et d'un noyau à l'autre.
3. **`stream_ms` est déduit du COMPTE d'échantillons, jamais d'une horloge.**
   Le rappel temps réel n'a pas le droit de lire l'heure. Un compteur de temps
   pris dans le fil du sondeur mentirait précisément quand la machine sature,
   c'est-à-dire au seul moment qui nous intéresse.
4. **Il ne se mélange pas à l'underrun ALSA.** Ce sont deux choses :
   l'*underrun ALSA*, remonté par cpal en `StreamError`, parle du pilote, est
   routinier, et ne dit pas si l'audio a sauté ; la *famine de l'anneau* dit que
   des zéros sont **partis vers le DAC**. Les cumuler rendrait inexploitable le
   chiffre censé décider du sort du noyau.

### Ce qui n'est PAS établi

* **`/proc/asound`.** Le ticket d'origine demandait « le compteur de xruns ALSA
  (`/proc/asound`) ». Je n'ai pas pu vérifier ce que ce répertoire expose sur
  une machine réelle : la machine de compilation n'a ni carte son ni
  `/proc/asound`. Le protocole ci-dessous **ne s'appuie donc pas dessus**. Si
  quelqu'un établit qu'un compteur cumulatif y existe sur noyau standard, il
  devient un second chiffre utile — pas un remplaçant.
* **Aucun compteur de sous-alimentation matérielle n'existe dans le dépôt.** Ce
  qui est mesuré est *au-dessus* de la couche pilote.
* **Aucune mesure de terrain n'a été faite.** Il faut une semaine de parc réel
  sur ≥ 0.9.131 avant de conclure quoi que ce soit sur `PREEMPT_RT`.

## Où le chiffre se lit

| source | portée | remise à zéro |
|---|---|---|
| `GET /api/v1/system/diagnostics` → `ring_starvation[]` | par sortie | **à chaque piste** |
| `GET /api/v1/devices/{id}/buffer-stats` et `/devices/buffer-stats/all` | par sortie | **à chaque piste** |
| rapport de bogue (`/api/v1/system/bug-report`) | par sortie | **à chaque piste** |
| journal : `famine_anneau_debut` / `famine_anneau_fin` | par zone, **daté**, cumulatif par épisode | jamais |

⚠️ **Les compteurs des routes repartent de zéro à la piste suivante.** Une
lecture d'une heure sur un album de douze pistes ne laisse donc, dans
`diagnostics`, que le relevé de la douzième. Un relevé pris à la fin d'une
session dit « 0 événement sur 0 servi (0 ms de flux) » si plus rien ne joue —
et ce zéro-là ne veut rien dire.

**Le seul relevé cumulatif sur une heure est le JOURNAL.** Le sondeur écrit deux
lignes par épisode, jamais plus, quelle qu'en soit la durée :

```text
WARN famine_anneau_debut zone_id=… device=… flux_ms=… rappels_a_court=… silence_ms=…
WARN famine_anneau_fin   zone_id=… device=… flux_ms=… rappels_a_court=… \
                         echantillons_manquants=… silence_ms=… duree_ms=…
```

## Le protocole

**Une seule machine, deux noyaux.** Comparer deux machines ne répond pas à la
question posée.

1. Machine Tune OS, noyau **standard**. Serveur ≥ 0.9.131.
2. Une seule zone, sortie **locale** (une zone réseau n'a pas d'anneau à
   affamer et ne mesure rien ici — voir plus bas).
3. Réglage de tampon **noté et laissé tel quel** entre les deux passes.
4. Une heure de lecture continue, même contenu, même format, sans interaction.
   Prendre un contenu représentatif de l'usage : si la question porte sur le
   hi-res, mesurer en hi-res.
5. Relever :
   * `grep -c famine_anneau_fin` sur la fenêtre d'une heure → **le nombre
     d'épisodes** ;
   * la somme des `silence_ms` de ces lignes → **le silence total envoyé au
     DAC** ;
   * la somme des `duree_ms` → la largeur d'audio couverte, qui doit approcher
     3 600 000 ms. Un écart franc veut dire que la lecture s'est arrêtée : la
     mesure est à refaire, pas à interpréter.
6. Redémarrer sur le noyau **`PREEMPT_RT`**, sans rien changer d'autre, et
   refaire 4 et 5.

### La décision

| noyau standard | conclusion |
|---|---|
| 0 épisode sur une heure, plusieurs machines | le noyau RT est un coût sans gain : on le retire, le Secure Boot revient. Cyclictest ne sert à rien. |
| épisodes non nuls | comparer `silence_ms/heure` entre les deux noyaux. **C'est seulement ici que cyclictest entre en jeu**, pour expliquer un écart, jamais pour le constater. |

### Trois pièges à ne pas retourner en résultat

* **Une zone réseau ne mesure rien.** Elle reçoit un flux déjà encodé, elle n'a
  pas d'anneau, et ses compteurs valent `null`. Un `null` n'est pas un zéro.
* **Un zéro sans dénominateur n'est pas un zéro.** Vérifier que `served_samples`
  et `stream_ms` sont non nuls avant de lire `events`. « 0 sur 0 » veut dire
  « rien n'a joué ».
* **La machine de mesure doit être au repos.** Un scan de bibliothèque, une
  passe ReplayGain ou une compilation en fond produisent de la famine qui ne
  doit rien au noyau.

## Ce que ce lot a corrigé pour rendre la mesure possible

`GET /devices/buffer-stats/all` et `GET /devices/{id}/buffer-stats` publiaient
`"total_underruns": 0` **écrit en dur**, sur toute sortie et en toutes
circonstances (`tune-server/src/routes/devices.rs`). C'est exactement le chiffre
dont dépend la décision ci-dessus, et il était fabriqué : une campagne qui
aurait lu ces routes aurait conclu au retrait du noyau RT sans avoir rien
mesuré.

Ces routes rendent désormais :

* `null` quand la sortie n'observe pas sa famine (tout renderer réseau, et toute
  sortie hors-arbre, dont le trait rend `None` par défaut) ;
* le compteur réel sinon — `0` y redevient une information : « mesuré, et rien
  n'a manqué ».

`total_disconnections` passe à `null` pour la même raison : aucun compteur de
déconnexion n'existe dans l'arbre.

Gardé par `tune-server/tests/famine_mesuree_3205.rs`.
