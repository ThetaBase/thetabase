# theta-mcp

The Model Context Protocol server for ThetaBase. JSON-RPC 2.0 over stdio.

## Why this exists

ThetaBase's primary user is an agent, and MCP is how an agent reaches a tool. A
CLI needs something to decide to shell out and parse the result; an MCP server
puts the operations in the model's tool list. That is the difference between a
database an agent *can* use and one it *does*.

## Install

```jsonc
// Claude Desktop / Claude Code — mcp config
{
  "mcpServers": {
    "thetabase": {
      "command": "theta-mcp",
      "env": {
        "THETA_ADDRESS": "127.0.0.1:7777",
        "THETA_TOKEN": "…"       // or run under `theta exec`, which injects it
      }
    }
  }
}
```

Running it under `theta exec` is the better path: the token reaches the process
through the environment the CLI already injects and is never written to a file
anybody manages.

## What it exposes

| Tool | |
|---|---|
| `theta_describe` | what is in here and where it came from — **ask this first** |
| `theta_query` | typed query, bound parameters |
| `theta_get` / `theta_put` / `theta_delete` | single rows |
| `theta_propose_schema_change` | propose; returns the gate and why |
| `theta_change_status` | the state of a proposal you made |
| `theta_review_queue` | what is waiting for a human (read-only) |
| `theta_branch_create` / `_list` / `_merge` | branches |
| `theta_audit` | the forensic trail, ranked by risk |

## What it does not expose, and why

**`confirm`, `promote`, `reject`, `revoke`, `push_policy`, `discard_branch`.**

The Safety Layer's design is that a destructive change is *proposed* by an agent
and *answered* by a person. An MCP server is the agent's hands. Exposing
`confirm` here would mean an agent proposes a destructive change, is told it
needs a human, and is the human — every gate in the product becomes a two-call
formality.

These are not "not yet". They are human authority, and the absence is the
design. `HUMAN_ONLY` in `lib.rs` lists them with a reason each, and a test
asserts no tool name contains one, so adding it fails a test that explains
itself rather than shipping behind a reasonable-sounding commit message.

An agent that calls one gets a refusal that names the operation and says to ask
a person, rather than "unknown tool" — a confusing answer to a reasonable
question.

## State

Wired and tested against a live `thetad` over TCP
(`tests/against_a_live_instance.rs`).

Both tools are real:

- **`theta_query`** sends SQL as text with bound parameters; the *server*
  compiles it, so this crate links no parser and there is no raw-string path
  into an execution plan. Results come back as Arrow IPC and are decoded to rows
  here, bounded at 100 with `truncated` said explicitly — an agent that counts
  the rows it received and reports that number is reporting the limit.
- **`theta_review_queue`** reads the engine's own queue through a `reviewQueue`
  wire request added for it. It used to be served from the audit trail, which is
  a different question: the audit is what happened, the queue is what has not
  happened yet, and a reviewer reading an empty audit would conclude there was
  nothing to review.

Branch *names* resolve to ids here, because a name is what an agent knows. A name
that does not resolve is refused and the refusal lists the branches that exist —
falling back to main is how a write lands on the wrong branch, which is the
failure this product is about.
