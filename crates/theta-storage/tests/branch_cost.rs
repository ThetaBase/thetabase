//! What branching actually costs, measured on the two axes BranchBench names.
//!
//! # Why this file exists
//!
//! Columbia's DAP Lab published *"Branchable Databases Aren't Ready for Agentic
//! Workloads"* (May 2026) with a benchmark, **BranchBench**, over Postgres,
//! DoltgreSQL, Neon, TigerData and Xata. Their finding is a trade-off nobody in
//! that set escapes:
//!
//! > systems optimized for fast branching suffer up to 5–4000× slower reads as
//! > branches deepen, while systems optimized for fast data operations incur
//! > 25–1500× higher branch creation and switching latency.
//!
//! **BranchBench itself cannot be run against ThetaBase, and not for want of
//! effort.** Its backend contract is a PostgreSQL connection: the base class
//! issues `information_schema` queries and `DROP DATABASE`, and every CRUD and
//! DDL operation calls `execute_sql(query: str)` with `SELECT * FROM …`,
//! `ALTER TABLE …`, `CREATE INDEX …`. Writing that adapter means giving ThetaBase
//! a door that takes a string and executes it — which is `docs/INVARIANTS.md` invariant 4,
//! the one thing the query IR must never gain. The benchmark is not measurable
//! here because of a property this database is built to have.
//!
//! So this measures BranchBench's two axes on ThetaBase's own surface. **These
//! numbers are not comparable to theirs** — different operations, different
//! host, different workload — and no page may present them as though they were.
//! What they can honestly say is which side of the trade-off ThetaBase lands on.
//!
//! # The answer, and it is not free
//!
//! Reads do not degrade with branch depth, because a branch is not a chain to
//! walk: `DurableLogStore::seed_branch_view` copies the parent's materialised
//! view once, at fork time, and every read afterwards is a lookup in the
//! branch's own map. Depth is not in the read path at all.
//!
//! The bill arrives at the fork instead. That copy is O(rows in the parent), so
//! branch creation off a large branch is proportionally expensive — ThetaBase is
//! squarely on the "fast data operations, costly branch creation" side of the
//! trade-off the paper describes, and this file measures how costly rather than
//! implying it is not.
//!
//! Run with `--release --nocapture`.

use std::time::Instant;

use theta_core::{Author, BranchId, CommitId, ContentHash, LogEntry, OpType, Value};
use theta_storage::wal::WalConfig;
use theta_storage::{BranchViews, DurableLogStore};

/// Skipped in a debug build, where this would measure rustc's bounds checks
/// rather than the engine — and report numbers wrong by an order of magnitude
/// in the reassuring direction.
fn release_only() -> bool {
    cfg!(debug_assertions)
}

fn put(prev: ContentHash, n: u64, branch: BranchId, key: &str, value: i64) -> LogEntry {
    LogEntry {
        prev_hash: prev,
        commit_id: CommitId(n),
        branch_id: branch,
        op: OpType::Put {
            key: key.into(),
            value: Value::Int(value),
        },
        author: Author::System,
        timestamp_ms: n as i64,
    }
}

fn fork(prev: ContentHash, n: u64, branch: BranchId, name: &str) -> LogEntry {
    LogEntry {
        prev_hash: prev,
        commit_id: CommitId(n),
        branch_id: branch,
        op: OpType::BranchCreate {
            name: name.into(),
            from: prev,
        },
        author: Author::System,
        timestamp_ms: n as i64,
    }
}

/// A store with `rows` keys on main, and main's head.
fn seeded(dir: &std::path::Path, rows: u64) -> (DurableLogStore, BranchViews, ContentHash, u64) {
    let opened = DurableLogStore::open(WalConfig::new(dir)).expect("open");
    let (mut store, mut views) = (opened.store, opened.views);

    let mut head = ContentHash::ZERO;
    for i in 0..rows {
        let entry = put(head, i, BranchId::MAIN, &format!("row:{i:07}"), i as i64);
        head = store.append_and_apply(entry, &mut views).expect("append");
    }
    (store, views, head, rows)
}

/// BranchBench's first axis: does reading get slower as branches deepen?
///
/// A chain of forks, each from the last — the "deep chain" topology the paper
/// calls out. Then the same key is read at the root and at the tip.
#[test]
fn a_read_does_not_get_slower_as_branches_deepen() {
    if release_only() {
        eprintln!("skipped: debug build");
        return;
    }

    const ROWS: u64 = 2_000;
    const DEPTH: u64 = 500;

    let dir = tempfile::tempdir().expect("tempdir");
    let (mut store, mut views, mut head, mut commit) = seeded(dir.path(), ROWS);

    // A chain: branch 1 from main, branch 2 from branch 1, and so on.
    let mut branch = BranchId::MAIN;
    for depth in 1..=DEPTH {
        let child = BranchId(depth);
        store.set_head(child, head);
        let entry = fork(head, commit, child, &format!("depth-{depth}"));
        head = store.append_and_apply(entry, &mut views).expect("fork");
        commit += 1;
        branch = child;
    }

    const READS: u32 = 200_000;
    let key = format!("row:{:07}", ROWS / 2);

    let at_root = time_reads(&views, BranchId::MAIN, &key, READS);
    let at_tip = time_reads(&views, branch, &key, READS);

    eprintln!(
        "\nbranch depth vs read latency ({ROWS} rows, depth {DEPTH}, {READS} reads each)\n\
         \x20 depth 0     {at_root:.3} µs\n\
         \x20 depth {DEPTH}   {at_tip:.3} µs\n\
         \x20 ratio       {:.2}×\n\
         BranchBench reports 5–4000× on this axis for the systems it measured.\n",
        at_tip / at_root
    );

    // Generous, because this is a timing test on a shared host and the claim is
    // about an order of magnitude rather than a percentage. What would fail it
    // is a design where depth reaches the read path at all — a chain walked per
    // read, or a copy-on-write stack consulted in order.
    assert!(
        at_tip < at_root * 3.0,
        "reads degraded {:.1}× with depth; a branch is supposed to be a view, \
         not a chain to walk",
        at_tip / at_root
    );
}

fn time_reads(views: &BranchViews, branch: BranchId, key: &str, rounds: u32) -> f64 {
    let view = views.get(&branch).expect("the branch has a view");
    let start = Instant::now();
    let mut found = 0u64;
    for _ in 0..rounds {
        if view.get(key).is_some() {
            found += 1;
        }
    }
    let elapsed = start.elapsed().as_secs_f64() * 1e6 / rounds as f64;
    assert_eq!(found, rounds as u64, "the key was not there to read");
    elapsed
}

/// BranchBench's second axis. ThetaBase used to lose here; now it does not.
///
/// Forking clones the parent's view, and until M10.6 that meant deep-copying
/// three maps keyed by the same strings plus the index entries — 1.5µs per
/// realistic row, so a million-row branch took a second and a half. The maps
/// now sit behind `Arc` and the clone bumps refcounts, so **the fork is
/// constant time whatever the parent holds**.
///
/// The copy is not gone. It is deferred to the first write on the branch, which
/// is what the next test measures — and measuring the fork alone, now, would
/// flatter this the way measuring `Value::Int` with no schema flattered it
/// before.
#[test]
fn forking_is_constant_time_whatever_the_parent_holds() {
    if release_only() {
        eprintln!("skipped: debug build");
        return;
    }

    const SMALL: u64 = 200;
    const LARGE: u64 = 20_000;

    let small = fork_cost(SMALL);
    let large = fork_cost(LARGE);
    let realistic = realistic_fork_cost(LARGE);

    eprintln!(
        "
fork latency by parent size
           {SMALL} bare rows          {small:.0} µs
           {LARGE} bare rows        {large:.0} µs
           {LARGE} realistic rows   {realistic:.0} µs   5 fields, 2 indexes
           → all one fsync. Before M10.6 the last of these was ~30,000 µs.
         BranchBench reports 25-1500× higher branch creation for the systems
         that keep reads fast. This one keeps reads fast and does not.
"
    );

    // Constant time, within the variance of a single fsync on a shared host.
    // What would fail this is the fork copying something again — which is the
    // regression worth catching, because it is one `.clone()` away.
    let spread = realistic.max(large).max(small) / small.min(large).min(realistic);
    assert!(
        spread < 3.0,
        "fork latency varies {spread:.1}× with parent size and shape; something is being copied at fork time again. The maps behind `MaterializedView` are shared until written (M10.6) — check for a `.clone()` that stopped going through `Arc`."
    );
}

/// There is no deferred copy left to find.
///
/// This test used to assert the opposite. When the maps moved behind `Arc`, the
/// fork became constant time and the copy moved to the first write on a branch —
/// about 3,800µs on a 20,000-row parent. That was a real cost and it was
/// reported next to the fork number, because a page claiming constant-time forks
/// without it would have been describing half the operation.
///
/// Persistent maps removed it rather than moving it again (M10.6 item 5). A
/// write path-copies O(log n) nodes and shares the rest, so the first write to a
/// forked branch costs what the second one costs.
///
/// **The test inverted, and that is the point of having written it.** It caught
/// its own obsolescence twice — once when the fork stopped scaling, once here.
#[test]
fn the_first_write_to_a_branch_costs_what_any_write_costs() {
    if release_only() {
        eprintln!("skipped: debug build");
        return;
    }

    const ROWS: u64 = 20_000;
    const FORKS: u64 = 20;

    let dir = tempfile::tempdir().expect("tempdir");
    let (mut store, mut views, head, mut commit) = seeded(dir.path(), ROWS);

    let mut firsts = Vec::with_capacity(FORKS as usize);
    let mut seconds = Vec::with_capacity(FORKS as usize);

    for i in 0..FORKS {
        let child = BranchId(i + 1);
        store.set_head(child, head);
        let mut child_head = store
            .append_and_apply(fork(head, commit, child, &format!("w-{i}")), &mut views)
            .expect("fork");
        commit += 1;

        for sample in [&mut firsts, &mut seconds] {
            let entry = put(child_head, commit, child, &format!("new:{commit}"), 1);
            commit += 1;
            let start = Instant::now();
            child_head = store.append_and_apply(entry, &mut views).expect("write");
            sample.push(start.elapsed().as_secs_f64() * 1e6);
        }
    }

    let median = |mut v: Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
        v[v.len() / 2]
    };
    let first = median(firsts);
    let second = median(seconds);

    eprintln!(
        "
first write to a freshly forked branch ({ROWS}-row parent)
           first write    {first:.0} µs
           second write   {second:.0} µs
           difference     {:.0} µs   ≈ {:.3} µs/row
         Before persistent maps this difference was ~3,800 µs — the branch's own
         copy of the state, paid on demand. There is nothing left to defer.
",
        first - second,
        (first - second) / ROWS as f64
    );

    // Both are one fsync, so the tolerance is the variance of an fsync rather
    // than a percentage of a number that no longer exists. What would fail this
    // is a copy reappearing — 3,800µs on this parent, which is six fsyncs and
    // impossible to mistake for noise.
    assert!(
        first < second * 2.0 + 1_000.0,
        "the first write to a forked branch cost {first:.0}µs against          {second:.0}µs for the second, so something is being copied on demand again. A persistent map path-copies O(log n) nodes; check for a collection that went back to `BTreeMap`, or a `.clone()` that materialises one."
    );
}

/// Median fork latency off a parent holding `rows` bare rows.
fn fork_cost(rows: u64) -> f64 {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut store, mut views, head, commit) = seeded(dir.path(), rows);
    time_forks(&mut store, &mut views, head, commit)
}

/// Median fork latency off a parent holding `rows` five-field indexed rows.
///
/// The shape the bare case leaves out: a declared table, two single-column
/// indexes, and rows that are maps rather than integers. Before M10.6 this was
/// 6.7× the bare figure, and quoting the bare one would have flattered every
/// comparison built on it.
fn realistic_fork_cost(rows: u64) -> f64 {
    use theta_core::schema::{FieldDef, IndexDef, SchemaChange, TableDef};
    use theta_core::ValueType;

    let field = |name: &str, ty: ValueType| FieldDef {
        name: name.into(),
        ty,
        nullable: true,
        crdt: None,
        declared_at: None,
    };

    let dir = tempfile::tempdir().expect("tempdir");
    let opened = DurableLogStore::open(WalConfig::new(dir.path())).expect("open");
    let (mut store, mut views) = (opened.store, opened.views);

    let table = TableDef {
        name: "customers".into(),
        fields: [
            ("email", ValueType::Text),
            ("name", ValueType::Text),
            ("plan", ValueType::Text),
            ("seats", ValueType::Int),
            ("region", ValueType::Text),
        ]
        .into_iter()
        .map(|(n, t)| (n.to_string(), field(n, t)))
        .collect(),
        indexes: vec![
            IndexDef {
                name: "by_email".into(),
                columns: vec!["email".into()],
                unique: true,
            },
            IndexDef {
                name: "by_plan".into(),
                columns: vec!["plan".into()],
                unique: false,
            },
        ],
    };

    let mut commit = 0u64;
    let mut head = store
        .append_and_apply(
            LogEntry {
                prev_hash: ContentHash::ZERO,
                commit_id: CommitId(commit),
                branch_id: BranchId::MAIN,
                op: OpType::Schema {
                    change: SchemaChange::AddTable { table },
                },
                author: Author::System,
                timestamp_ms: 0,
            },
            &mut views,
        )
        .expect("declare");
    commit += 1;

    for i in 0..rows {
        let row = Value::Map(
            [
                ("email", Value::Text(format!("user{i:07}@example.com"))),
                ("name", Value::Text(format!("Customer Number {i}"))),
                (
                    "plan",
                    Value::Text(if i % 3 == 0 { "team" } else { "pro" }.into()),
                ),
                ("seats", Value::Int((i % 50) as i64 + 1)),
                ("region", Value::Text("eu-west".into())),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
        );
        head = store
            .append_and_apply(
                LogEntry {
                    prev_hash: head,
                    commit_id: CommitId(commit),
                    branch_id: BranchId::MAIN,
                    op: OpType::Put {
                        key: format!("customers:{i:07}"),
                        value: row,
                    },
                    author: Author::System,
                    timestamp_ms: commit as i64,
                },
                &mut views,
            )
            .expect("append");
        commit += 1;
    }

    time_forks(&mut store, &mut views, head, commit)
}

/// Median of forty forks off `head`, timed around the append.
///
/// Around the append rather than around the view copy, because the append is
/// what a caller waits on — the fsync and whatever the fork does, together.
fn time_forks(
    store: &mut DurableLogStore,
    views: &mut BranchViews,
    head: ContentHash,
    mut commit: u64,
) -> f64 {
    const FORKS: u64 = 40;

    let mut samples = Vec::with_capacity(FORKS as usize);
    for i in 0..FORKS {
        let child = BranchId(i + 1);
        store.set_head(child, head);
        let entry = fork(head, commit, child, &format!("wide-{i}"));
        commit += 1;

        let start = Instant::now();
        store.append_and_apply(entry, views).expect("fork");
        samples.push(start.elapsed().as_secs_f64() * 1e6);
    }

    samples.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    samples[samples.len() / 2]
}

/// The third thing BranchBench times, which ThetaBase does not really do.
///
/// "Switching" a branch is indexing a map. There is no connection to re-open,
/// no working set to warm, and nothing to check out — so the number is reported
/// for completeness and the honest statement is that this operation barely
/// exists here.
#[test]
fn switching_branches_is_a_map_lookup() {
    if release_only() {
        eprintln!("skipped: debug build");
        return;
    }

    const ROWS: u64 = 2_000;
    const BRANCHES: u64 = 200;
    const SWITCHES: u32 = 200_000;

    let dir = tempfile::tempdir().expect("tempdir");
    let (mut store, mut views, head, mut commit) = seeded(dir.path(), ROWS);

    for i in 0..BRANCHES {
        let child = BranchId(i + 1);
        store.set_head(child, head);
        let entry = fork(head, commit, child, &format!("wide-{i}"));
        store.append_and_apply(entry, &mut views).expect("fork");
        commit += 1;
    }

    let start = Instant::now();
    let mut reached = 0u64;
    for i in 0..SWITCHES {
        let branch = BranchId(u64::from(i) % BRANCHES + 1);
        if views.contains_key(&branch) {
            reached += 1;
        }
    }
    let per_switch = start.elapsed().as_secs_f64() * 1e9 / SWITCHES as f64;
    assert_eq!(reached, SWITCHES as u64);

    eprintln!(
        "\nbranch switch ({BRANCHES} branches, {SWITCHES} switches)\n\
         \x20 {per_switch:.0} ns per switch\n"
    );
}
