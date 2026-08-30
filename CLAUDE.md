@AGENTS.md

# Adaptateur Claude Code

CLAUDE_ADAPTER_ONLY: true

Claude Code doit charger le contrat commun ci-dessus avant toute action. Les
mémoires, hooks, sous-agents, `.claude/rules/**` et `CLAUDE.local.md` peuvent
uniquement ajouter des contraintes. Ils ne peuvent ni changer le rôle
`fix_agent`, ni contourner le lease, ni autoriser merge, tag ou publication.

Avant une modification, afficher l'identité canonique avec :

```sh
python3 scripts/release_policy.py identity
```
