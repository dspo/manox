# TS Pi fixtures

Stable artifacts captured from the TypeScript Pi implementation, used by
Rust differential tests. Each fixture records the TS SHA it was generated
from; the Rust tests consume only committed fixtures.

Baseline: `4488ad55c18f07ae89a489096c90de8667b3adfb`

Refresh: run `scripts/refresh_ts_pi_fixtures.sh` with `PI_TS_REPO` pointing at
a checkout of the TS Pi monorepo, then commit the updated fixtures. The
script is a manual, reviewed step; CI only reads what is committed.
