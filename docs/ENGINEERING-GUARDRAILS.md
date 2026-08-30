# Garde-fous d'ingénierie Tune Server

POLICY_SUPPORT_DOC: non-authoritative

Ce document conserve les précautions techniques utiles de l'ancien
`CLAUDE.md`. Il ne définit ni permissions, ni topologie de branches, ni
procédure de release : `.github/release-policy.json` est seule autoritaire sur
ces sujets.

## Avant de modifier le code

1. Mettre les refs à jour et chercher le correctif par contenu sur `main`, les
   RC et les PR fusionnées. Un numéro d'issue n'est pas une preuve d'absence.
2. Travailler dans un worktree isolé créé depuis la branche d'intégration
   explicitement choisie.
3. Lire les versions depuis les refs distantes, jamais depuis une copie de
   travail potentiellement ancienne.
4. Acquérir le lease d'issue puis, si nécessaire, les verrous de zone.

## Changements de schéma

Une colonne doit être traitée dans les quatre chemins suivants :

1. `CORE_SCHEMA` pour une base SQLite neuve ;
2. la migration SQLite pour une base existante ;
3. `PG_FULL_SCHEMA` pour une base PostgreSQL neuve et le transfert SQLite vers
   PostgreSQL ;
4. la migration PostgreSQL enregistrée dans `PG_MIGRATIONS` pour une base
   existante.

Si la copie SQLite vers PostgreSQL lit la colonne, le bloc de rattrapage de
`PG_FULL_SCHEMA` doit aussi la connaître. Les migrations SQLite utilisent
`add_column_if_missing` au lieu d'un `ALTER TABLE ADD COLUMN` inconditionnel.

## Validation

- Tester la régression ciblée et sa contre-épreuve.
- Préférer `cargo clippy --all-targets` à `cargo check` seul lorsque le graphe
  concerné le permet.
- Capturer le vrai code de sortie de la commande testée. Un `echo` final ou un
  pipeline peut masquer un échec.
- Une cross-compilation macOS ne prouve ni le runtime, ni la signature, ni la
  notarisation sur macOS.

## Suivi des issues

Une PR fusionnée n'est pas une preuve de livraison. Une issue n'est déclarée
corrigée qu'après vérification du contenu de l'actif public correspondant ;
les issues `keep-open` restent ouvertes. Les réponses aux testeurs sont
coordonnées pour éviter les doublons et doivent citer la première version qui
contient effectivement le correctif.
