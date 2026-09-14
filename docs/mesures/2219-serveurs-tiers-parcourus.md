# Serveurs tiers parcourus — ce qu'ils publient réellement

Troisième livrable de la **phase 0**. Le chantier demandait de parcourir des serveurs tiers
réels et de relever, pour chacun : forme de l'`ObjectID`, présence de `res@size`,
`SystemUpdateID` réel, `SearchCapabilities`, temps de parcours, comportement à la réindexation.

Mesuré le **14/09/2026**. Serveurs désignés par Bertrand : `.42`, `.15`, et **Asset UPnP sur
le Mac Studio** (`.41`). Les Sonos sont écartés — ils sont vides, cf
[`2219-pourquoi-les-sonos-manquent.md`](2219-pourquoi-les-sonos-manquent.md) §6.

## Asset UPnP — le seul serveur vraiment tiers

`Illustrate Ltd`, `Asset UPnP Server`, `friendlyName = "Asset UPnP: Mac-Studio-6"`,
descripteur sur `http://192.168.1.41:26125/DeviceDescription.xml`.

**Racine = `urn:schemas-upnp-org:device:MediaServer:1`**, avec `ContentDirectory` directement
dessus. C'est exactement le contraire de Sonos, et c'est pourquoi Tune saurait le classer —
une fois découvert.

### Les relevés

| question | Asset UPnP | Tune (pour mémoire) |
|---|---|---|
| `SystemUpdateID` | **42** — il bouge vraiment | **1**, figé (cf D5) |
| `SearchCapabilities` | **`*`** — tout est cherchable | à mesurer |
| `SortCapabilities` | **vide** | — |
| `res@size` | **présent** — `776552` sur la piste témoin | à mesurer |
| racine | **12 conteneurs** en `0,01 s` | — |

Les douze conteneurs de la racine : `Album Artist`, `Album`, `Title`, `Composer`, `Genre`,
`Dynamic Browsing`, et six autres. « Album » contient **28 entrées**, parcourues en `0,03 s`.

### La forme des `ObjectID` — le point qui compte pour D6

Deux préfixes, deux formes :

```
conteneur : coAC11F0152318872C
piste     : d7239415746361406494-coAC11F0152318872C
```

* les conteneurs portent `co` + **16 hexadécimaux** ;
* une piste porte un identifiant décimal long, **suivi de l'identifiant de son conteneur
  parent**. L'identité d'une piste est donc *contextuelle* : la même piste vue sous « Album »
  et sous « Genre » ne portera pas le même `ObjectID`.

C'est un avertissement direct pour la phase 2 : `source_id = '<udn>|<objectid>'` **ne
suffirait pas** à dédoublonner. Deux entrées pour le même fichier sont la norme, pas
l'exception, dès qu'un serveur propose plusieurs axes de navigation.

Ces identifiants ne sont **pas** dérivés d'un `rowid` : ils ressemblent à des condensats. Leur
stabilité à la réindexation reste **non mesurée** — c'est la contre-épreuve que le chantier
réclame, et elle demande de forcer une réindexation d'Asset.

### Ce que `res@` publie

```
protocolInfo, size, duration, bitrate, bitsPerSample, sampleFrequency,
nrAudioChannels, ORG_PN, ORG_OP, ORG_CI, ORG_FLAGS
```

**Tout ce dont la phase 2 a besoin est là** : taille, durée, profondeur, cadence, canaux. Rien
à déduire, rien à aller chercher par une seconde requête.

### Le temps de parcours

`0,01` à `0,03 s` par niveau, sur une bibliothèque modeste. **Ce n'est pas une mesure de
charge** : il faudrait un catalogue de plusieurs dizaines de milliers de pistes pour que le
chiffre veuille dire quelque chose. À refaire sur la vraie bibliothèque.

## `.42` et `.15` — des Tune, pas des tiers

Le M-SEARCH les identifie comme `Tune/0.9.148` et `Tune/0.9.120`. Ils valent comme
**comparaison entre versions** — notamment pour D5, puisque `.15` en 0.9.120 est antérieure à
toute correction du `SystemUpdateID` — mais ils ne renseignent pas sur les conventions d'un
serveur tiers.

## Ce qui reste à mesurer

1. **La stabilité des `ObjectID` d'Asset à la réindexation** — la contre-épreuve du chantier.
   Si elle est stable, la phase 2 change de forme.
2. **Le temps de parcours sur un vrai catalogue**, pas sur celui-ci.
3. **Un second serveur tiers** d'une autre famille — MinimServer, Twonky ou LMS — pour savoir
   si la forme contextuelle des `ObjectID` d'Asset est une convention répandue ou sa
   particularité. **Un seul tiers ne fait pas une règle.**

## Une note de méthode

Le M-SEARCH lancé depuis ce Mac **ne voit pas Asset**, alors qu'il tourne dessus : un hôte ne
reçoit pas ses propres réponses multicast. Asset n'est donc visible que depuis une autre
machine — et il l'est bien : le registre du `.18` contient `192.168.1.41`.

⚠️ Mais il l'a enregistré sous `name = "Tune Server"`, `manufacturer = "MozAIk Labs"` — ce qui
est **faux** pour Asset. Soit le Mac fait aussi tourner un Tune sur le même hôte et le
registre n'en garde qu'un, soit l'identité est écrasée. À élucider : cela touche directement
la D1bis, qui veut afficher le nom du serveur.
