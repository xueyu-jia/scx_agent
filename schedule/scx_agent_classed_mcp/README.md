# scx_agent_classed_mcp

`scx_agent_classed_mcp` is the standalone MCP adapter for the
`scx_agent_classed` scheduler. It translates tuning-agent MCP requests into the
versioned control protocol owned by the scheduler project.

## Boundary

The processes and dependencies are deliberately one-way:

```text
tuning-agent -> MCP stdio -> scx_agent_classed_mcp
                              -> Unix control socket
                              -> scx_agent_classed
```

This project depends only on the scheduler's `control-api` crate. It does not
depend on BPF, `scx_utils`, or scheduler runtime internals. The scheduler is the
only process that writes the learned-rule file and publishes rules to BPF.

## Run

Start the scheduler with a persistent rule file and control socket, then point
the MCP process at that socket:

```bash
scx_agent_classed_mcp \
  --control-socket /run/scx_agent_classed/control.sock \
  --journal /var/lib/scx_agent_classed/mcp-operations.json
```

Configure tuning-agent with an absolute executable path:

```toml
[activation]
socket_path = "/run/tuning-agent/activation.sock"

[mcp]
enabled = true

[[mcp.servers]]
id = "scx-agent-classed"
enabled = true
command = "/usr/local/libexec/scx_agent_classed_mcp"
args = [
  "--control-socket", "/run/scx_agent_classed/control.sock",
  "--journal", "/var/lib/scx_agent_classed/mcp-operations.json",
]
request_timeout_ms = 30000
allowed_capabilities = [
  "rules.snapshot.v1",
  "rule.upsert.v1",
  "classification.integrity.v1",
]
allow_mutations = true
```

The provider exposes bounded rule snapshots, reversible single-`comm` upserts,
and classification integrity measurements. Mutation recovery state is stored
in the journal passed with `--journal`.

## Build and test

From this project directory:

```bash
cargo fmt --check
cargo test
cargo build --release
```

The stdio end-to-end test opens a Unix socket and may be blocked in restricted
build sandboxes. Unit and wire-contract tests do not require a running
scheduler.
