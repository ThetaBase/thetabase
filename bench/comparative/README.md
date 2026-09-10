# Comparative branch benchmark

```sh
make bench-comparative
```

Measures BranchBench's axes on a workload ThetaBase, Dolt and PostgreSQL can all
express — single-table key-value — because BranchBench itself cannot run against
ThetaBase. Its backend contract is a PostgreSQL connection and every operation
calls `execute_sql(query: str)`, which is the door `docs/INVARIANTS.md` invariant 4 says
the query IR must never gain.

**Read `docs/COMPETITION.md` §4a before quoting anything from this.** The
supported claim is narrow — branch creation, where ThetaBase is 3.7× faster than
Dolt and 19× faster than PostgreSQL and does not scale with parent size. Two of
the four axes are losses, the read-depth degradation the paper reports did not
reproduce at this scale, and the numbers say nothing about throughput, joins,
concurrency, or anything at scale.

The six rules the harness is built around are in its module docstring. The one
that matters most: **the same boundary on every system.** ThetaBase goes through
its Python SDK to a live `thetad` over a socket, not in-process — an in-process
number next to a competitor's over-the-wire one would be the easiest and least
honest way to win this.

## Setup

```sh
docker run -d --name thetabase-bench-pg -p 55440:5432 \
  -e POSTGRES_PASSWORD=bench -e POSTGRES_USER=bench -e POSTGRES_DB=bench postgres:16

docker run -d --name thetabase-bench-dolt -p 55441:3306 \
  -v $PWD/bench/comparative/dolt-init.sql:/docker-entrypoint-initdb.d/init.sql \
  dolthub/dolt-sql-server:latest

pip install psycopg2-binary pymysql
```

`dolt-init.sql` is not optional: the Dolt image creates `root@localhost` only,
and a connection through Docker's port map is not localhost.

## Results

`results/` holds raw JSON per run, dated. Keep them — a comparison that cannot
be re-run is an assertion, and one whose old numbers were overwritten cannot be
checked for drift.
