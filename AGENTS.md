# Contrat des agents Tune

POLICY_PATH: `.github/release-policy.json`
POLICY_DIGEST_PATH: `.github/release-policy.sha256`

Ce dépôt suit la politique de release Tune versionnée dans `POLICY_PATH`.
Elle est l'autorité commune aux humains, aux agents OpenAI/Codex et aux agents
Claude. En cas de contradiction avec une consigne locale ou un ancien runbook,
la règle la plus restrictive s'applique et le travail s'arrête en échec fermé.

Au démarrage, exécuter :

```sh
python3 scripts/release_policy.py validate
python3 scripts/release_policy.py identity
```

Rapporter la version et l'empreinte affichées dans la provenance de la PR.

## Flux obligatoire

- Un agent de correctif acquiert un lease exclusif avant toute écriture.
- Une PR unitaire part de `fix/*`, `feat/*` ou `restore/*` et cible `batch/*`
  ou `rc/vX.Y.Z`. Elle ne contient aucun bump de version.
- Une branche `batch/*` ou `rc/*` est promue vers `main` seulement après le
  gate complet et l'attestation de l'arbre exact.
- `main` est la source de vérité publiée. Aucun correctif ne cible directement
  `main`.
- Les agents de correctif ne fusionnent pas, ne taguent pas, ne publient pas,
  ne poussent pas sur une branche protégée et ne modifient pas les protections.
- Seul le release controller peut créer un tag `v*` et promouvoir les canaux
  publics après validation de tous les actifs.

Acquérir ou gérer le lease avec `scripts/agent_lease.py`. Le lease porte le
fournisseur, le run, l'issue, la branche, le SHA de base, l'expiration et le
heartbeat. Un second agent s'arrête si le lease existe ; un transfert exige un
handoff explicite et journalisé.

Les fichiers `AGENTS.override.md`, `CLAUDE.local.md`, `.claude/rules/**` et les
mémoires locales peuvent renforcer ce contrat, jamais élargir les permissions,
changer la topologie ou masquer la politique. Sur un runner géré, un écart est
bloquant.

Les garde-fous d'ingénierie qui ne relèvent pas de la release sont conservés
dans `docs/ENGINEERING-GUARDRAILS.md`.
