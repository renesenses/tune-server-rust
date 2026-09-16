# LMS, SlimProto et conflits de ports

Tune peut à la fois servir des platines SlimProto et piloter les platines
d'un LMS/Lyrion externe. Ces deux rôles ont des réglages distincts.

| Rôle de Tune | Port par défaut | Réglage |
| --- | --- | --- |
| Serveur SlimProto, canal de lecture TCP et découverte UDP | 3483 | Variable `TUNE_SLIMPROTO_PORT` |
| Pont de commande texte LMS, TCP | 9090 | Variable `TUNE_CLI_PORT` |
| Client d'un LMS externe | 9090 sur le LMS | Adresse LMS dans les réglages Squeezebox, avec `:port` si nécessaire |

Les variables sont lues au démarrage de Tune. Les définir dans l'environnement
du processus qui lance Tune, puis redémarrer le serveur. Un exemple Linux,
si ces ports sont libres :

```sh
TUNE_SLIMPROTO_PORT=3484 TUNE_CLI_PORT=9091 tune-server
```

`.env.tune.example` décrit aussi ces variables. Changer le port de Tune
n'altère pas l'adresse du LMS externe dans les réglages. Si Lyrion tient
déjà 9090, le client de Tune peut atteindre Lyrion alors que le pont CLI de
Tune échoue à démarrer sur ce même numéro.

Sur un port SlimProto différent de 3483, certains lecteurs n'utilisent pas
la découverte automatique : leur configurer manuellement l'adresse **et le
port** de Tune, lorsque le lecteur le permet. Adapter également le port des
contrôleurs qui utilisent le pont CLI. Ne pas arrêter un autre service sans
savoir à qui il appartient.

## Distinguer les trois écoutes

La grille de composants de `GET /api/v1/system/health` expose :

- `slimproto` : écoute du canal de lecture TCP ;
- `lms_cli` : écoute du pont de commande TCP ;
- `slimproto_udp` : écoute de la découverte UDP.

Un échec CLI ou UDP ne dégrade pas le verdict global de santé de la base.
Avant toute tentative, ces deux composants sont absents ; ils disparaissent
aussi lorsque leur écoute est arrêtée. Une découverte volontairement désactivée
ne devient donc pas une panne. Un échec de bind reste consultable jusqu'à
l'arrêt explicite ou une nouvelle tentative.

`GET /api/v1/system/diagnostics/network`, l'état Squeezebox et le rapport de
bogue exposent le port réel, le protocole, `ecoute`, `cause`, `message` et
`erreur_systeme`. Une erreur UDP ne prouve ni un échec TCP ni l'identité du
service qui occupe le port. Lire les états séparément.

## LMS répond sans annoncer de platine

`GET /api/v1/squeezebox/status` reste HTTP 200 lorsque LMS répond.
Un recensement vide porte `diagnostic.code = lms_sans_platine`. Un recensement
illisible porte `lms_recensement_impossible` ; une réponse invalide n'est
plus interprétée comme zéro lecteur.

Le bouton de découverte appelle `POST /api/v1/squeezebox/discover`.
Si LMS répond sans platine, cette action renvoie HTTP 409 avec le code
`lms_sans_platine` et un message dans `error`, déjà lu par le client web.
Une découverte avec des platines garde son succès HTTP 200. Le recensement
automatique en arrière-plan conserve son succès vide et sa journalisation.

Un recensement vide ne dit pas pourquoi : lecteur éteint, absent ou pont mal
configuré. Vérifier les platines dans LMS. Pour HQPlayer, vérifier le pont qui
doit le présenter comme platine à LMS. Changer le port d'écoute de Tune ne
peuple pas la liste d'un LMS externe.

Ces diagnostics couvrent le volet serveur de #3462. L'écran d'ouverture et
l'avatar relèvent des tickets client ; les équipements du testeur ne sont pas
reproduits par les tests de sockets locaux.
