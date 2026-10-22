# Écoute à distance par laissez-passer : contrat entre le relais, le serveur et le site

Référence : #6603. Le format du laissez-passer, du jeu de clés et des défis est
fixé par `tune-laissez-passer/vecteurs/README.md` ; ce document fixe les
échanges.

## App → relais (`tune-bridge`)

| Route | Corps | Réponse |
|---|---|---|
| `GET /session/defi/{server_id}` | — | `{"defi", "expire"}` (60 s, usage unique) ; 404 `serveur_absent` |
| `POST /session/{server_id}` | `{"laissez_passer", "defi", "signature"}` | `{"jeton", "expire", "secours"}` |
| `POST /session/renouveler` | idem, jeton de session en `X-Bridge-Token` | même jeton, `expire` prolongé |

Le jeton de session (256 bits, base64url) s'utilise exactement comme l'ancien
jeton du pont : `X-Bridge-Token` sur l'API, `?token=` pour les flux et le
WebSocket. Il ne change pas au renouvellement.

Refus : JSON `{"code", "motif"?, "appareil"?}`.

| Statut | `code` | Sens |
|---|---|---|
| 401 | un code de refus du laissez-passer (`expire`, `signature`, `defi`…) | voir `vecteurs/README.md` |
| 401 | `non_autorise` | aucun jeton reconnu |
| 401 | `session_coupee` | `motif` : `ecoute_reprise_ailleurs` (avec `appareil`), `revoquee`, ou le motif du site |
| 401 | `session_expiree` | laissez-passer non renouvelé |
| 401 | `lien_obsolete` | ancien lien d'accès, chemin hérité fermé |
| 401 | `autre_appareil` | renouvellement par un autre compte, appareil ou clé |
| 403 | `refuse_par_le_site` | `motif` : celui du site |
| 403 | `revoque` | appareil ou laissez-passer révoqué |
| 503 | `cloud_unavailable` | site injoignable et règle de secours non remplie |
| 503 | `jeu_de_cles_absent` | aucun jeu de clés valable |

Une session coupée ferme ses flux audio en cours, et son WebSocket avec le
code `4001` (raison : le JSON ci-dessus).

Pendant la coexistence, l'ancien jeton du pont reste accepté avec l'en-tête
`X-Tune-Avertissement: lien-obsolete`. `TUNE_BRIDGE_JETON_HERITE=0` le ferme.

## Relais → site (`auth.tune-bridge`, `TUNE_BRIDGE_SERVICE_TOKEN`)

`POST /api/v1/remote/sessions`

```json
{"jti": "…", "sub": "…", "did": "…", "sid": "…", "chemin": "relais", "secours": false}
```

Réponse : `{"ok": true, "session_id": "…", "couper": ["session_id…"], "appareil": "iPhone de …"}`
ou `{"ok": false, "motif": "…"}` (un 4xx portant ce corps est lu comme un
refus ; tout le reste comme une panne). `secours: true` : session ouverte
pendant une panne du site, rejouée à son retour.

`POST /api/v1/remote/heartbeat`, chaque minute s'il y a quelque chose à dire :

```json
{"sessions": [{"session_id": "…", "octets_depuis_le_dernier": 0, "audio_actif": true}],
 "jeton_herite": {"<server_id>": 12}}
```

`jeton_herite` : requêtes servies par l'ancien lien depuis le battement
précédent, par serveur. Aucune IP, aucun titre.

Réponse : `{"couper": [{"session_id", "motif"}], "revoques": {"jti": [], "did": []}, "jeu_de_cles": <version>}`.

`GET /api/v1/remote/keys` (publique) : `{"jeu", "minisig"}`.

`GET /api/v1/remote/servers/{server_id}/key` : `{"public_key": "…"}`, ou 404
si le site n'en connaît pas. Quand une clé est connue, le relais refuse tout
enregistrement de ce `server_id` qui ne la prouve pas.

## Une écoute à distance par compte

Une session compte comme écoute dès son premier `/stream/relay`, et cesse de
compter après 2 min sans audio. Quand une session se met à jouer, le relais
coupe aussitôt les autres sessions du même compte qui jouaient
(`ecoute_reprise_ailleurs`). Le site fait de même entre relais et direct, par
`couper`.

## Secours (site injoignable)

- laissez-passer valide : accepté, ouverture rejouée au retour du site ;
- laissez-passer expiré depuis moins de 24 h, appareil ouvert avec l'accord
  du site depuis moins de 7 jours : accepté, `secours: true` ;
- tout le reste : 503 `cloud_unavailable`.

## Serveur → relais

`relay.register` porte `server_pk` (Ed25519, base64url). Le relais répond
`relay.challenge {"defi"}` ; le serveur répond `relay.challenge_response
{"signature"}` (message `tune-remote/serveur/v1`). `relay.registered` porte
`jeton_herite` (l'ancien lien est-il encore accepté) et `laissez_passer`.

## Serveur → site

`POST /api/v1/remote/servers/{server_id}/key`, jeton OAuth du compte relié :
`{"public_key", "alg": "Ed25519", "preuve"}` (message
`tune-remote/inscription/v1`). La graine privée reste dans le coffre du
serveur (`credentials_vault`, service `ecoute_distante_cle_serveur`).

## Serveur, Réglages

- `GET /cloud/bridge/status` : `acces_par_compte` (texte à afficher),
  `lien_obsolete: true`, `lien_herite_coupe`.
- `GET /cloud/bridge/access-link` : pendant la coexistence, le lien avec
  `obsolete: true` et `message` ; après la coupure annoncée par le relais,
  410 `lien_obsolete`.
- `POST /cloud/bridge/enable` : ne rend plus `bridge_token` après la coupure.
