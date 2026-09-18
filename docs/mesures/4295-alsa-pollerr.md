# #4295 — récupération POLLERR de la sortie ALSA

JP Robbe / OpenAI Codex / jp-robbe-20260918-suite6-4295.

## Périmètre

Base de PR : af70d7e251735d8be2c5d7ddc9d6539f61e2ac34,
lot batch/jp-sdk-premium-20260917. Ce rattachement regroupe le manifeste
partagé avec #4364 ; il n'introduit aucune dépendance fonctionnelle aux plugins.
CPAL, tune-core/Cargo.toml et le moteur local y sont identiques à main5bef7771.
Seules les versions du workspace distinguaient les manifestes des deux bases.
Aucun commit de main ni bump n'est repris.

CPAL reste 0.17.3, sous vendor/cpal avec licence et provenance.
L'ajout exclude est nécessaire : la fixture Cargo isolée avec un [workspace]
dans la dépendance échoue par « multiple workspace roots found ».
Le conflit de cette ligne avec l'ajout sdk de #4364 est identifié : conserver
les deux exclusions à l'intégration, sans reprendre le SDK dans cette PR.

Adaptation limitée à la sortie ALSA : voir vendor/cpal/TUNE-PATCH.md.
L'entrée audio conserve son chemin antérieur. local.rs et l'orchestrateur,
réservés par #2219, ne sont pas modifiés.

## Validation

Worktree Shrek : /srv/builds/worktrees/jp-4295-20260918-suite6.
Clé target propre : jp-4295-20260918-suite6.
Preuves : /srv/builds/jp-evidence/jp-4295-20260918-suite6.

Commande ciblée :
cargo test -p tune-core --test alsa_poll_recovery_4295 --no-default-features --features local-audio -- --test-threads=1

Les témoins utilisent le vrai worker CPAL et le vrai poll système, un PCM null
ALSA privé et une injection uniquement aux frontières ALSA. LD_PRELOAD n'est
passé qu'au sous-processus du test. Pas de matériel, compte ou serveur réel.

### Résultats obtenus

- Premier passage : 7 verts / 2 rouges. Les sorties anticipées après déconnexion
  fermaient le récepteur du pipe ; Drop échouait sur assert(ret == 8).
  Le prérequis amont de propriété Arc conserve désormais ce récepteur dans
  Stream. Aucune assertion de réveil n'a été supprimée.
- Second passage : 8 verts / 1 rouge. La fixture injectait parfois EPIPE dans
  un cycle déjà réveillé, avant son POLLERR ; prepare effaçait ensuite la panne.
  L'injection EPIPE attend maintenant le premier événement POLLERR. Les
  assertions n'ont pas été assouplies.
- Passage corrigé : 9/9 verts (green-3.log, 2,83 s de tests).
- Contre-épreuve : remplacement de TOUT src/host/alsa/mod.rs par la source
  CPAL 0.17.3 amont, mêmes tests et même fixture. Compilation réussie (56 s),
  puis 1 vert / 8 rouges (18,12 s, sortie Cargo 101). Le témoin sain
  healthy_null_pcm_still_plays_and_drops reste vert.
- Exemples de messages rouges : « POLLERR recovery was not attempted by the
  actual worker », « transient POLLERR blocked a running PCM »,
  « disconnection not classified by actual worker ». Il ne s'agit pas d'un
  échec de compilation.
- Restauration par cp de alsa.fixed.rs, puis sha256sum -c : les trois fichiers
  production/test/fixture sont identiques au passage vert.
- Dernier passage restauré : 9/9 verts (restored-green.log, 1,59 s).
- cargo fmt --all --check, rustfmt --edition 2021 --check sur le backend
  vendored et git diff --check : verts.
- Clippy sur tune-core --lib et la cible de test, même graphe, avec
  -D clippy::correctness : vert (3 min 58 s). 450 avertissements sur des
  fichiers tune-core inchangés, 2 sur tune-output-api inchangé ; aucun sur
  la nouvelle cible. Ce contrôle ne prétend pas lint-er toutes les
  dépendances vendored.

Les deux premiers passages rouges ont servi à corriger le prototype et la
fixture. Ils ne sont pas présentés comme la contre-épreuve formelle.

Empreintes du passage vert et de la restauration :

- ALSA mod.rs : 1c8b262a59a4baad1c2ada449a65705a561fe92c6592d1695893aa2cf282d1d6
- Test Rust : 91fcaebdf9e158b8f59f51389ecadb9499779bf735c923e62afef64f0c38f238
- Fixture C : e99672fe2876a918c561669ddfe09056ef4e5cdcf9a486e35efe6b17f5329545

Les 65 autres fichiers de la crate sont identiques octet par octet à la
source du cache dont l'archive SHA-256 correspond au Cargo.lock initial.
Les seules additions/modifications vendored sont le backend ALSA et
TUNE-PATCH.md.

## CI et limites

ci:full requis. La cible tune-core doit apparaître dans les journaux du job
audio-embedding, qui sélectionne tune-core avec ses fonctionnalités par défaut
dont local-audio. Le job shipped-features ne sélectionne pas tune-core : sa
réussite ne prouverait pas l'exécution de ces tests.

Pas d'identification de la cause initiale des POLLERR USB/DAC du terrain.
Pas de validation matérielle ALSA, Windows ou macOS. Aucun traitement de file,
de reconnexion aveugle, ni de modification du DSP ou de capture.
