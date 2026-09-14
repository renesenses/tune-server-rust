# Pourquoi les deux Sonos ne sont pas dans le registre du `.18`

Deuxième livrable de la **phase 0**. Le document de chantier posait la question ainsi :

> établir **pourquoi le registre `media_servers` du `.18` est à 84 194 s** et n'a pas les deux
> Sonos. Sans ce point, aucune phase ultérieure ne repose sur rien.

Mesuré le **14/09/2026**, le `.18` tournant en **v0.9.149**.

## 1. Les 84 194 s ont disparu — la phase 1 a réglé ce point

`GET /api/v1/network/media-servers` rend aujourd'hui `absent_apres_secs = 5400` (90 min), et
les trois entrées ont un `last_seen_secs` de l'ordre de **2 000 s**. Le registre est devenu
durable et daté en absolu, comme la phase 1 le promettait.

La première moitié de la question est donc close, **par le correctif, pas par l'analyse**.

## 2. Les Sonos, eux, manquent toujours — et ce n'est pas un problème de réseau

### Ce que le réseau porte réellement

M-SEARCH `ST: urn:schemas-upnp-org:device:MediaServer:1` depuis le Mac, **cinq** réponses :

| adresse | serveur |
|---|---|
| 192.168.1.15 | `Tune/0.9.120 UPnP/1.0` |
| 192.168.1.18 | `Tune/0.9.149 UPnP/1.0` |
| **192.168.1.19** | **`Linux UPnP/1.0 Sonos/86.8-78270 (ZPS1)`** |
| **192.168.1.20** | **`Linux UPnP/1.0 Sonos/86.8-78270 (ZPS1)`** |
| 192.168.1.42 | `Tune/0.9.148 UPnP/1.0` |

Le registre du `.18` n'en contient que **trois**, tous des Tune. Les deux Sonos **répondent
bien** au M-SEARCH, et leur descripteur est joignable (HTTP 200).

### La forme du descripteur Sonos

```
deviceType, dans l'ordre du document :
  0: urn:schemas-upnp-org:device:ZonePlayer:1      ← racine
  1: urn:schemas-upnp-org:device:MediaServer:1     ← imbriqué
  2: urn:schemas-upnp-org:device:MediaRenderer:1   ← imbriqué
```

`ContentDirectory` existe bel et bien — mais il appartient au **MediaServer imbriqué**, pas à
la racine.

## 3. La cause, dans le code

`tune-core/src/discovery/xml_parser.rs:401` — quand la racine n'est ni un renderer ni porteuse
d'`AVTransport`, le parseur cherche parmi les appareils imbriqués **le renderer**, et lui seul :

```rust
if !desc.is_media_renderer() && !desc.has_av_transport() {
    let renderer = embedded_devices.iter().find(|d| d.is_media_renderer())
        .or_else(|| embedded_devices.iter().find(|d| d.has_av_transport()))
        .cloned();
    if let Some(renderer) = renderer { … desc.services.extend(renderer.services); }
}
```

**Le MediaServer imbriqué n'est jamais rattaché.** Le commentaire du bloc dit son intention
sans détour — il a été écrit pour faire marcher les appareils composites **en tant que
renderers** (#2072). La moitié serveur n'a simplement jamais été envisagée.

Puis, dans `ssdp.rs:1389`, la classification est une **chaîne de `else if`** :

```
1. is_openhome()        → non  (racine = ZonePlayer)
2. is_media_renderer()  → non  (le type de la RACINE ne contient pas « MediaRenderer »)
3. has_av_transport()   → OUI  ← les services du renderer imbriqué viennent d'être rattachés
4. is_media_server()    → JAMAIS ÉVALUÉ
```

Le Sonos est donc **découvert**, mais rangé comme **renderer DLNA**. Il n'a aucune chance
d'entrer dans `media_servers` : la branche qui l'y mettrait est en aval d'un `else`.

### En une phrase

**Un appareil qui est à la fois serveur et renderer ne peut être que l'un des deux**, et la
chaîne choisit toujours renderer.

Ce n'est pas particulier à Sonos : tout appareil composite déclarant une racine non standard
(ZonePlayer, MediaServer imbriqué) subit le même sort — BubbleUPnP Server, certains NAS,
plusieurs box opérateur.

## 4. Ce qu'il faudrait changer, et à quel prix

Trois gestes, indépendants :

1. **rattacher aussi le `MediaServer` imbriqué**, pas seulement le renderer — le commentaire
   de #2072 prévient qu'un `ConnectionManager` de serveur écraserait celui du renderer, dont
   le `Sink` deviendrait vide. Il faut donc **fusionner en conservant les deux**, pas
   `extend` aveuglément ;
2. **remplacer la chaîne de `else if` par une classification multiple** : un appareil peut
   légitimement être renderer *et* serveur, et le dépôt le modélise déjà ainsi pour les
   zones ;
3. **chercher `is_media_server()` dans l'arbre entier**, pas seulement à la racine.

Aucun des trois n'est risqué pris isolément. Le second est celui qui change une structure de
données publique, et mérite d'être fait en premier pour que les deux autres aient où atterrir.

## 5. Une anomalie annexe, non expliquée

Les trois serveurs du registre portent `presence: "present"` **et** `reachable: false`. Or les
trois répondent HTTP 200 sur leur `description.xml` depuis le Mac, à l'instant de la mesure.

Soit `reachable` mesure autre chose que la joignabilité HTTP — un parcours `Browse` réussi,
par exemple —, soit il n'est jamais remis à `true`. **Non tranché ici** : cela demande de lire
le calcul de ce champ, ce qui sort du périmètre de cette mesure.

## 6. ⛔ Les Sonos ne serviront PAS de serveur tiers de référence

**Arbitrage de Bertrand, 14/09/2026 : « ne prends pas les Sonos, ils sont vides. »**

Un Sonos Play:1 expose bien un `ContentDirectory`, mais ce n'est pas une bibliothèque : il y
publie sa file de lecture, ses favoris et ses services, pas un catalogue de fichiers. Le
parcourir ne renseignerait ni sur la forme des `ObjectID` d'un vrai serveur, ni sur
`res@size`, ni sur le temps de parcours d'un catalogue réel.

**Conséquence pour la phase 0** : les trois serveurs tiers à mesurer restent à trouver
ailleurs — MinimServer, Asset ou Twonky, Synology DS Audio, LMS. Les Sonos sont hors liste, et
ce paragraphe existe pour qu'on ne les y remette pas.

**Conséquence pour ce qui précède : aucune.** Le défaut de classification décrit plus haut
reste entier et vaut d'être corrigé — il ne concerne pas que Sonos. Tout appareil composite à
racine non standard est rangé comme renderer et ne peut jamais être serveur, y compris ceux
qui, eux, portent une vraie bibliothèque.
