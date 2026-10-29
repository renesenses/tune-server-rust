# #7263 — fins de vie Linux et garde du parc audio

Relecture du 28 octobre 2026 des deux journaux exportés (1000 lignes chacun),
des deux diagnostics et de la capture du fil 2491. Version établie : rc8.1.
Les pièces privées ne sont pas ajoutées au dépôt.

| Journal | Dernière ligne de l'ancienne vie (+01:00) | Début du démarrage suivant (+01:00) |
| --- | --- | --- |
| 27 octobre | 18:44:49.408 | 18:51:28.170 |
| 28 octobre | 10:33:19.720 | 10:36:37.158 |

Dans les deux cas, la dernière opération visible est l'énumération ALSA :
`local_audio_enumerating_devices`, puis les greffons `lavrate`, `samplerate`
et `speexrate`. Il manque ensuite `local_audio_devices_enumerated`.
Les précédentes énumérations complètes du même extrait vont plus loin.

Le 28, FIP joue vers une zone réseau. Aucun marqueur d'arrêt propre, de gel
de l'exécuteur ou de panique n'est consigné avant le redémarrage. Cela ne
prouve pas leur absence : la panique Rust Linux va à stderr et le journal
noyau n'est pas dans ces exports. Le RSS avant l'arrêt est de 2076 Mo au
27/10 18:43:53, et de 1519 Mo au 28/10 10:29:46 ; il ne démontre pas un OOM.
Les restitutions de mémoire à froid sont visibles dans les journaux.
Les trois autres arrêts rapportés le 27 ne sont pas dans ces extraits.

## Défaut établi dans le code

Le tag serveur `v1.0.0-rc8.1` porte déjà cette différence de gardes.
Le rescan périodique Linux évite les sondes locales pendant **toute** lecture,
y compris réseau. Les routes `/devices/audio` et `/system/diagnostics`
passent par `parc_local_sans_couper_la_lecture`, qui ne vérifiait que les
sorties locales. Elles pouvaient donc contourner cette protection pendant
FIP et rouvrir les greffons ALSA. La protection est alignée sur le rescan :
cache tant qu'une zone joue sous Linux ; le hotplug reprend à l'arrêt.
Windows et macOS conservent leur règle portant sur la sortie locale.

Le témoin compte les appels à l'énumérateur avec une radio réseau en lecture,
puis à l'arrêt ; aucun matériel ni son n'est utilisé. La contre-épreuve doit
retirer uniquement la garde réseau de production et faire rougir ce témoin
sur Linux, avant restauration par copie et retour au vert.

## Ce qui reste à établir

Ce correctif ne prouve **pas** la cause des deux fins de vie. Un blocage ou
un signal natif dans une sonde ALSA est une piste, pas un diagnostic de JACK.
Il faut le signal/coredump, ou la sortie terminal, aux heures précises
ci-dessus, ainsi que le journal noyau pour distinguer un crash d'un OOM.
Le 27, les relevés portent `stream_sessions=0` : la nouvelle garde de lecture
ne supprime pas les sondes à l’arrêt et ne prétend donc pas prévenir ce cas.
Aucune nouvelle lecture de FIP ni reproduction sonore n'a été effectuée.
