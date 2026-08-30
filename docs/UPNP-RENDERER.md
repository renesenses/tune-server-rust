# Piloter Tune depuis JPlay, BubbleUPnP ou mconnect

*Mode d'emploi du **renderer UPnP** de Tune — la fonction qui permet à un point
de contrôle (JPlay iOS, BubbleUPnP, mconnect…) d'**envoyer la lecture VERS une
zone Tune**.*

> Disponible depuis la **v0.9.79**. Le réglage est **désactivé par défaut** et
> s'active **zone par zone** : tant que la case n'est pas cochée, aucune zone
> n'apparaît dans JPlay. C'est la cause n°1 de « je ne vois pas mon Tune ».

---

## 1. Deux rôles à ne pas confondre

Tune expose **deux** appareils UPnP distincts, qui ne servent pas à la même
chose. La question « est-ce que JPlay voit Tune ? » n'a pas la même réponse
selon celui dont on parle.

| | MediaServer | MediaRenderer |
|---|---|---|
| **Sens** | JPlay **lit** la bibliothèque Tune | JPlay **envoie** le son vers Tune |
| **Ce que c'est** | Le serveur, une seule fois | Une **sortie**, une par zone |
| **Activation** | Actif par défaut | **Opt-in, par zone** |
| **`deviceType`** | `…:device:MediaServer:1` | `…:device:MediaRenderer:1` |

Si vous cherchez à **choisir Tune comme sortie** dans JPlay, c'est le
**MediaRenderer** qu'il faut activer — c'est l'objet de cette page.

Les deux sont **indépendants** : les renderers de zone sont annoncés même si le
MediaServer a été désactivé (`upnp_enabled` à `false`). Couper le serveur ne
retire donc pas les sorties, et inversement.

---

## 2. Activer la fonction

### Depuis l'interface web (recommandé)

1. **Réglages** → onglet **« Appareils »** ;
2. repérer la carte de la zone à rendre pilotable ;
3. cocher **« Renderer UPnP »**.

L'infobulle de la case résume la fonction :

> La zone s'annonce sur le réseau comme sortie UPnP : JPlay, BubbleUPnP ou
> mconnect peuvent y envoyer la lecture. Le flux traverse toute la chaîne Tune
> (EQ, convolveur, trim).

La case est **par zone** : cocher pour la zone « Salon » n'expose pas la zone
« Cuisine ». C'est voulu — on ne publie pas sur le réseau des sorties que
personne n'a demandées.

**L'annonce SSDP part immédiatement**, sans redémarrage : cocher la case
réveille l'annonceur. La zone doit apparaître dans JPlay en quelques secondes.

### Depuis l'API

```bash
# Activer
curl -X PATCH http://<tune>:8888/zones/<id> \
     -H 'Content-Type: application/json' \
     -d '{"upnp_renderer": true}'

# Désactiver
curl -X PATCH http://<tune>:8888/zones/<id> -d '{"upnp_renderer": false}'
```

L'état est relu dans `GET /zones` (champ `upnp_renderer`).

En base, le réglage vit dans la clé `zone_{id}_upnp_renderer`. La clé est
**supprimée** à la désactivation, jamais mise à `"false"` : l'absence de clé et
le défaut désarmé sont un seul et même état.

---

## 3. Ce que JPlay voit une fois la case cochée

La zone apparaît dans la liste des **sorties** (pas des serveurs) sous le nom :

```
<nom de la zone> (Tune)
```

Le suffixe `(Tune)` est ajouté volontairement : il distingue la zone Tune de
l'appareil physique qu'elle pilote, qui peut porter le même nom et s'annoncer
lui aussi sur le réseau.

Fiche d'identité annoncée :

| Champ | Valeur |
|---|---|
| `deviceType` | `urn:schemas-upnp-org:device:MediaRenderer:1` |
| `friendlyName` | `<nom de la zone> (Tune)` |
| `manufacturer` | MozAIk Labs |
| `modelName` | Tune |
| `X_DLNADOC` | DMR-1.50 |
| `UDN` | stable, mémorisé (`upnp_renderer_udn_{id}`) |

L'UDN est **persistant** : il ne change pas d'un redémarrage à l'autre, donc
JPlay retrouve la sortie qu'il avait mémorisée au lieu d'en créer une nouvelle.

### Services publiés

- **AVTransport:1** — `SetAVTransportURI`, `SetNextAVTransportURI`, `Play`,
  `Pause`, `Stop`, `Seek`, `GetTransportInfo`, `GetPositionInfo`,
  `GetMediaInfo` ;
- **RenderingControl:1** — `GetVolume`, `SetVolume`, `GetMute`, `SetMute`
  (canal `Master`) ;
- **ConnectionManager:1** — `GetProtocolInfo` (le `Sink` liste les formats
  acceptés), `GetCurrentConnectionIDs`, `GetCurrentConnectionInfo`.

### Ce qui se passe à la lecture

Le flux reçu **traverse toute la chaîne Tune** : EQ, convolveur, trim de gain,
multiroom. Ce n'est pas un tunnel : c'est le même chemin que n'importe quelle
lecture Tune, avec `source = "upnp"`.

Le volume de JPlay agit sur le **volume de la zone**, et le mute sur son mute.

---

## 4. Limites connues

À lire avant d'ouvrir un ticket — ces points sont des choix ou des manques
identifiés, pas des pannes.

- **Pas d'événements GENA (`LastChange`).** L'abonnement `SUBSCRIBE` est
  accepté et rend un SID valide, mais **aucune notification n'est émise**. Les
  points de contrôle qui suivent l'état en interrogeant `GetTransportInfo` /
  `GetPositionInfo` — c'est le cas de JPlay — fonctionnent normalement. Un
  point de contrôle qui s'appuierait **uniquement** sur les événements verrait
  un état figé.

- **`Next` / `Previous` ne sont pas implémentés.** Ces actions rendent une
  faute SOAP `401 Invalid Action`. L'enchaînement se fait par
  `SetNextAVTransportURI`, que les points de contrôle utilisent en pratique.

- **Enchaînement sans blanc** (`SetNextAVTransportURI`) : disponible depuis la
  **v0.9.80**, pas dans la 0.9.79.

- **`SetAVTransportURI` interrompt la lecture en cours** sur la zone. Envoyer
  depuis JPlay vers une zone qui joue déjà coupe ce qu'elle jouait — même
  arbitrage que pour une entrée AirPlay ou Spotify Connect.

- **PCM nu non accepté.** Le `Sink` annonce FLAC, WAV, MP3, AAC/MP4, OGG et
  Opus, mais **pas `audio/L16`** : un PCM sans en-tête ne porte ni cadence ni
  profondeur lisibles au fil de l'eau.

- **Annonce en IPv4 uniquement.** Le SSDP du renderer est diffusé sur
  `239.255.255.250:1900` en IPv4.

- **Pas de `ssdp:byebye` à la désactivation.** Décocher la case arrête les
  annonces, mais ne retire pas activement l'appareil : un point de contrôle
  peut continuer d'afficher la sortie jusqu'à l'expiration du cache
  (`max-age`, 30 min). Redémarrer la recherche dans JPlay l'efface plus vite.

- **Cycle d'annonce de 10 minutes** en régime établi. Une activation ou une
  désactivation ne l'attend pas : elle réveille l'annonceur tout de suite.

---

## 5. La zone n'apparaît pas dans JPlay

Dans l'ordre, du plus fréquent au plus rare :

1. **La case n'est pas cochée.** C'est le cas de loin le plus courant : la
   fonction est désactivée par défaut. Vérifier zone par zone —
   `GET /zones` doit rendre `"upnp_renderer": true` pour la zone visée.

2. **Le point de contrôle regarde la mauvaise liste.** Une zone Tune est une
   **sortie**, pas un serveur. Dans JPlay, elle apparaît là où l'on choisit
   l'appareil de lecture, pas dans la liste des bibliothèques.

3. **Le multicast ne passe pas.** VPN actif, Wi-Fi avec isolation des clients,
   ou deux sous-réseaux différents : le SSDP n'atteint pas l'iPhone. Mettre le
   téléphone et le serveur sur le même réseau, et couper le VPN le temps du
   test.

4. **L'IP annoncée n'est pas joignable.** Sur une machine à plusieurs
   interfaces (VPN, Docker, plusieurs cartes), Tune peut annoncer une adresse
   que le point de contrôle ne peut pas atteindre. La forcer avec le réglage
   **`advertised_ip`**.

   Le diagnostic se lit dans le journal, qui indique la `LOCATION` sur
   laquelle la récupération a échoué.

5. **Vérifier à la main que la description répond.** Depuis le réseau du
   téléphone :

   ```bash
   curl -i http://<tune>:8888/upnp/renderer/<zone_id>/description.xml
   ```

   - `200` + XML contenant `MediaRenderer:1` → le renderer est bien publié, le
     problème est côté réseau ou point de contrôle ;
   - **`404`** → la zone n'a **pas** l'opt-in activé (ou l'identifiant de zone
     est faux). Retour au point 1.

---

## 6. Pour aller plus loin

- Lire la bibliothèque Tune **depuis** JPlay relève du MediaServer, pas de
  cette page.
- Le chemin du signal appliqué au flux reçu (EQ, convolveur, trim) est celui de
  n'importe quelle lecture Tune.
