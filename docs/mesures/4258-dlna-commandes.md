# #4258 — mesurer les commandes DLNA et rendre les refus SOAP

Identité : JP Robbe / OpenAI Codex / jp-robbe-20260917-p1parallel-4258.

Lot visé : `batch/jp-p1-dlna-20260917` (même SHA de base).

Base : `73707a08c1658289913058a9843250622be08521`.
Worktree : `/srv/builds/worktrees/jp-4258-20260917-parallel`.
Target : `/srv/cache/tune/targets/jp-4258-20260917-parallel`.

## Observation et périmètre

Le commentaire du 17 septembre sur #4258 identifie le Salon comme DLNA Rygel.
Son journal ne contient aucune mesure des commandes Pause/Play réussies.
Cette absence empêche de dater l'appel DLNA et d'en mesurer la réponse ; elle
ne démontre pas que la commande n'est jamais partie.

Le code présente également un défaut reproductible : le transport retourne
le corps des fautes SOAP pour laisser `play_media` gérer les refus 701 et ses
reprises. `pause` et `resume` ignoraient ce corps et retournaient un succès,
même pour un refus 701. Ces deux méthodes propagent désormais ce refus.
Le corps brut reste disponible pour les reprises existantes de `play_media`.

## Lire les mesures

Chaque appel AVTransport `Pause` ou `Play` produit :

- `dlna_command_sending` : début de l'appel logique, `action`, `command_id`,
  `device` et `device_id` ;
- `dlna_command_finished` : mêmes identifiants, `elapsed_ms` et `outcome`.

Les résultats sont `response_received`, `soap_fault` (code UPnP lorsqu'il est
disponible) et `transport_error`. La durée monotone inclut les réessais et
la redécouverte éventuelle. Le journal existant fournit l'heure murale.
Chaque nouvel appel reçoit un identifiant distinct à l'échelle du processus.
Ni le corps SOAP ni une URL de média ne sont ajoutés aux journaux.

`response_received` décrit seulement la réception de la réponse HTTP sans faute SOAP reconnue : ce n'est pas une validation formelle d'un acquittement SOAP,
ni une preuve d'arrêt acoustique ou une mesure de l'état du renderer. Le clic
client et l'attente avant l'appel de la sortie ne sont pas instrumentés.
Une ligne de début sans fin ne permet pas de conclure entre appel encore
en attente, annulation de la tâche ou arrêt du serveur.

Les sondages `GetTransportInfo` ne produisent pas ces lignes ; aucune requête,
attente ou stratégie de reprise supplémentaire n'est introduite.

## Validation

Exécuté sur Shrek, `CARGO_BUILD_JOBS=6` et environnement Tune chargé :

```sh
cargo test -p tune-core --lib command_tests_4258 --no-default-features --features oaat
cargo test -p tune-core --lib outputs::dlna --no-default-features --features oaat
cargo fmt --all -- --check
cargo clippy -p tune-core --lib --no-default-features --features oaat -- -D clippy::correctness
```

- Les 7 nouveaux tests passent.
- Après contre-épreuve et restauration : 118 tests DLNA passent, dont les
  7 nouveaux (aucun ignoré), en 2,69 s hors compilation.
- Format et Clippy correctness : réussis. Clippy émet 434 avertissements
  hors des lignes modifiées ; aucun correctif automatique appliqué.

Les fixtures HTTP utilisent exclusivement le loopback : aucun appel au Salon
ni à un compte externe. Le délai est contrôlé par deux notifications :
le test attend la réception de Pause, constate le début sans résultat,
puis libère la réponse après 80 ms. Il ne dépend pas d'une course de timers.
Les témoins exercent la véritable sortie DLNA, capturent ses journaux, vérifient
les refus SOAP (HTTP 500 et Fault namespacée HTTP 200), les réponses positives,
le HTTP 500 vide, les identifiants concurrents et le maintien du corps brut
pour la reprise Play existante. Les sondages restent silencieux.

### Contre-épreuve

Commande : la première commande ci-dessus, avec `dlna.rs` de la base
`73707a08` et uniquement la déclaration du module de tests ajoutée.
Le fichier des tests reste inchangé. Résultat : compilation réussie,
**0 vert / 7 rouges**, code de sortie 101. Exemples d'assertions :

- `pause_and_resume_propagate_soap_faults_instead_of_false_success` :
  `renderer SOAP 701 must reject pause/resume, not report success: ()` ;
- `namespaced_fault_in_http_200_is_not_an_acknowledgement` :
  `HTTP 200 does not turn a SOAP Fault into success` ;
- `pause_logs_start_before_reply_and_measures_soap_delay` :
  `start missing before response`.

Restauration par `cp` de la sauvegarde, vérification SHA-256 des fichiers
production **et** tests : identiques, puis les 118 tests au vert.
Le script reproductible et les journaux sont conservés dans
`/srv/builds/jp-evidence/jp-4258-20260917-parallel`.

La toute première exécution avait 6 rouges dus à une assertion de format de
journal incorrecte (`action=Pause` au lieu de `action="Pause"`).
Cette assertion a été corrigée avant le vert et avant la contre-épreuve ;
son journal est conservé comme `initial-tests.log` et ne constitue pas
la contre-épreuve.

## Limite de livraison

Cette PR est une correction du faux succès SOAP et une instrumentation du
diagnostic. Elle ne prétend pas résoudre la latence du premier Pause signalée
par FabienM. Les réponses XML malformées ou non SOAP ne sont pas traitées par ce changement.
Cela inclut un HTTP 500 avec corps HTML/texte, dont le transport conserve le
comportement préexistant ; seul le HTTP en échec sans corps est déjà rejeté.
Le ticket reste ouvert pour confrontation au prochain journal
terrain et à l'arrêt effectivement entendu.
