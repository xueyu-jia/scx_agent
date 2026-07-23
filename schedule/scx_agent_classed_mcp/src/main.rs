use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Result};
use clap::Parser;

use scx_agent_classed_mcp::provider::Server;
use scx_agent_classed_mcp::rpc::serve_stdio;

#[derive(Debug, Parser)]
#[command(about = "MCP provider for scx_agent_classed dynamic rules")]
struct Args {
    #[arg(
        long,
        env = "SCX_AGENT_CLASSED_CONTROL_SOCKET",
        default_value = "/run/scx_agent_classed/control.sock"
    )]
    control_socket: PathBuf,

    #[arg(
        long,
        env = "SCX_AGENT_CLASSED_MCP_JOURNAL",
        default_value = "/var/lib/scx_agent_classed/mcp-operations.json"
    )]
    journal: PathBuf,

    #[arg(
        long,
        env = "SCX_AGENT_CLASSED_CONTROL_TIMEOUT_MS",
        default_value_t = 5_000
    )]
    control_timeout_ms: u64,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("scx_agent_classed_mcp: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    if args.control_timeout_ms == 0 || args.control_timeout_ms > 300_000 {
        bail!("control timeout must be between 1 and 300000 ms");
    }
    let server = Server::new(
        args.control_socket,
        args.journal,
        Duration::from_millis(args.control_timeout_ms),
    );
    serve_stdio(&server)
}
