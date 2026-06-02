# Baseline performance — Tune v0.8.22

**Server** : http://192.168.1.15:8888
**Date** : 2026-06-02 12:44
**Library** : 22884 pistes, 1561 albums
**Server uptime** : 9826 secondes

**Total requests analyzed** : 1000
**Total errors** : 0
**Error rate** : 0.0%

## Top 10 endpoints les plus appelés

| # | Endpoint | Count | Avg ms | P50 | P95 | P99 | Max |
|---|----------|-------|--------|-----|-----|-----|-----|
| 1 | `GET /zones/31` | 143 | 0.0 | 0 | 0 | 1 | 1 |
| 2 | `GET /zones/10` | 143 | 0.0 | 0 | 0 | 1 | 1 |
| 3 | `GET /zones/14` | 143 | 0.0 | 0 | 0 | 0 | 0 |
| 4 | `GET /zones/3` | 142 | 0.1 | 0 | 1 | 1 | 1 |
| 5 | `GET /zones/13` | 142 | 0.1 | 0 | 1 | 1 | 1 |
| 6 | `GET /zones/16` | 142 | 0.0 | 0 | 0 | 0 | 0 |
| 7 | `GET /zones/11` | 142 | 0.1 | 0 | 1 | 1 | 1 |
| 8 | `GET /system/health/monitor` | 3 | 3.0 | 3 | 3 | 3 | 3 |

## Top 10 endpoints les plus lents (P95)

| # | Endpoint | Count | Avg ms | P50 | P95 | P99 | Max | Errors |
|---|----------|-------|--------|-----|-----|-----|-----|--------|
| 1 | `GET /system/health/monitor` | 3 | 3.0 | 3 | 3 | 3 | 3 | 0 |
| 2 | `GET /zones/3` | 142 | 0.1 | 0 | 1 | 1 | 1 | 0 |
| 3 | `GET /zones/13` | 142 | 0.1 | 0 | 1 | 1 | 1 | 0 |
| 4 | `GET /zones/11` | 142 | 0.1 | 0 | 1 | 1 | 1 | 0 |
| 5 | `GET /zones/31` | 143 | 0.0 | 0 | 0 | 1 | 1 | 0 |
| 6 | `GET /zones/16` | 142 | 0.0 | 0 | 0 | 0 | 0 | 0 |
| 7 | `GET /zones/10` | 143 | 0.0 | 0 | 0 | 1 | 1 | 0 |
| 8 | `GET /zones/14` | 143 | 0.0 | 0 | 0 | 0 | 0 | 0 |

## Conclusion

Ce rapport sert de baseline pour comparer la performance future après les optimisations.

Seuils indicatifs :

- **P95 < 100ms** : excellent
- **P95 < 500ms** : acceptable
- **P95 > 500ms** : à optimiser

