# #4324 — suivante UPnP rattachée à sa lecture

JP Robbe / OpenAI Codex / jp-robbe-20260917-4324-renderer-owner

## Problème et correctif

Le watcher regardait seulement si la zone avait été en lecture, puis arrêtée.
Une lecture native Tune pouvait donc lui faire consommer une ancienne suivante
UPnP. La route rattache maintenant la suivante au contexte SetURI et au
`play_seq` d'une lecture effectuée par le renderer. Le watcher abandonne
sa suivante si ce numéro, la source ou l'URI ne correspondent plus.

Un SetNext avant Play reste autorisé, sans adopter une lecture étrangère.
Stop UPnP et SetURI invalident le contexte ; Pause/Play et Seek UPnP
conservent ou actualisent l'appartenance. Un résultat Play marqué en erreur
(notamment remplacé par une commande plus récente) ne réarme pas le watcher.
L'inscription du watcher et sa sortie utilisent le même verrou que SetNext,
pour éviter de perdre un réarmement entre le constat « aucune suivante » et
la sortie. Une observation de playback ne peut invalider un contexte modifié
pendant sa lecture.

## Base et isolement

- Base `batch/p1-20260917` : `792b1aaeb671ee3a3036f7e25a139d539e6054af`.
- Branche `fix/jp-robbe-20260917-4324-renderer-owner`.
- Worktree Shrek `/srv/builds/worktrees/jp-4324-20260917`.
- Target `/srv/cache/tune/targets/jp-4324-20260917`, six jobs.
- Journaux `/srv/builds/jp-evidence/jp-4324-20260917`.
- Verrou global acquis atomiquement, conservé pendant revue.
- Aucun changement de migration, version, pipeline audio ou workflow.
- Pendant la validation, main a avancé à
  `73707a08c1658289913058a9843250622be08521` (.153).
  Le fichier renderer est identique entre cette tête et la base du lot.
  La prochaine intégration du lot dans une RC reste à décider par le mainteneur.

## Validation sur Shrek

Environnement de toutes les commandes :

```sh
export TUNE_TARGET_KEY=jp-4324-20260917 CARGO_BUILD_JOBS=6
. /srv/cache/tune/env.sh
```

```sh
cargo fmt --all -- --check
cargo test -p tune-server --lib --no-default-features \
  --features oaat,cloud-relay,bandcamp session_4324_tests -- --nocapture
cargo test -p tune-server --lib --no-default-features \
  --features oaat,cloud-relay,bandcamp routes::upnp_media_renderer -- --nocapture
cargo clippy -p tune-server --lib --tests --no-default-features \
  --features oaat,cloud-relay,bandcamp -- -D clippy::correctness
git diff --check
```

Les neuf nouveaux tests passent, puis **15 tests distincts** du renderer
passent après restauration du correctif (les neuf sont inclus dans les 15).
Formatage, Clippy avec -D clippy::correctness et git diff --check réussis.
Les avertissements existants hors périmètre restent visibles dans les journaux.
Les autres tests unitaires sont filtrés. Horloge Tokio contrôlée, vrai routeur
HTTP/SOAP, base SQLite dans un répertoire temporaire propre à chaque test, vrai orchestrateur et watcher,
sortie MockOutput. Les lectures natives utilisent la source podcast ou UPnP,
sans compte Qobuz ni serveur de musique réel.

Cas nouveaux :

1. reprise Tune puis arrêt : aucune injection de l'ancienne suivante ;
2. reprise et arrêt entre deux ticks : même protection ;
3. autre lecture de la même URI UPnP : le numéro de lecture distingue le propriétaire ;
4. fin de la bonne lecture : une seule avance, sans répétition aux ticks suivants ;
5. SetNext avant Play : attend une commande renderer, sans adopter la lecture native ;
6. Pause/Play : reprend sans rejouer et conserve l'enchaînement ;
7. Stop puis nouveau SetURI : l'ancienne observation ne déclenche rien ;
8. plusieurs SetNext : le dernier gagne, puis le watcher peut se réarmer ;
9. SetNext reçu après la reprise Tune : ne réarme pas le renderer périmé.

Une première compilation du banc a détecté E0716 (Arc temporaire verrouillé) ;
le helper a été corrigé. L'isolation de la sauvegarde de file a également
nécessité le TuneConfig du serveur, distinct de celui de tune-core (E0308
corrigé, journal config-type-error.log). Ces rouges de compilation ne sont
pas les contre-épreuves comportementales décrites ci-dessous.

## Contre-épreuve comportementale

Sauvegarde de la production, tests inchangés, puis désactivation de la seule
condition d'appartenance :

```rust
if false && (owner != ps.play_seq || !lecture_de_session(session, &ps)) {
```

Commande : celle filtrée par `session_4324_tests` ci-dessus.
Compilation réussie ; **4 échecs / 5 succès**, exit 101.

- `reprise_tune_puis_arret_ne_lance_pas_l_ancienne_suivante` :
  « #4324 : le watcher a injecté une ancienne suivante UPnP après l'arrêt Tune »,
  `left: 3, right: 2`.
- `reprise_et_arret_entre_deux_ticks_sont_detectes` : `left: 3, right: 2`.
- `meme_uri_rejouee_par_tune_n_est_plus_la_session_renderer` :
  « la comparaison d'URI seule accepte une autre lecture du même titre »,
  `left: 3, right: 2`.

- `set_next_tardif_apres_reprise_tune_ne_rearme_pas_le_renderer` :
  « #4324 : un SetNext tardif a repris une lecture appartenant a Tune »,
  `left: 3, right: 2`.

Restauration par `cp`, puis vérification SHA-256 :

- production : `6df0cf904450f9a5130ce78cf301065101a1bf1efd08904afb0e57cf193bc4d1` ;
- tests : `0e534c69e594dfb1d4737fcda5de4a2f400eace7a6e4f7d6da0e14205f8065a3`.

Les deux empreintes correspondent, puis les **15/15 tests du renderer sont verts**.
Journaux finaux : `final-green.log`, `final-counter.log`, `final-counter.exit`,
`final-restored.sha256`, `final-restored-green.log`, `final-fmt.log`,
`final-clippy.log`. Les journaux sans préfixe final gardent la première
validation avant isolation des fichiers de sauvegarde et ajout du neuvième cas.

## Limites et revue

- La cause de l'arrêt Qobuz à 21h52 et l'origine exacte du SetNext du rapport
  ne sont pas établies par ces tests.
- Sortie simulée : pas de validation C19/Diretta/JPlay ni d'écoute terrain.
- Un Stop natif de la même lecture UPnP, sans reprise préalable de la zone,
  reste indiscernable d'une fin de piste pour ce watcher.
- Le polling reste à deux secondes et l'avance utilise un Play complet :
  cette preuve ne mesure pas un zéro-gap audio.
- Ce changement ne sérialise pas toutes les commandes natives et UPnP dans
  l'orchestrateur. Les courses de commandes strictement simultanées entre
  contrôle de l'état et dispatch ne sont pas couvertes par cette validation.
- Les neuf tests sont inclus dans la bibliothèque tune-server, sans feature
  supplémentaire : le job Test habituel les sélectionne via `-p tune-server`
  avec `oaat,cloud-relay,bandcamp`. Les résultats GitHub restent séparés de
  ces preuves locales et ne remplacent pas la batterie de la prochaine RC.
- Aucun merge, tag, bump ou déploiement effectué.
