use std::path::PathBuf;

use tuning_agent::activation::source::send_unix_activation;
use tuning_agent::activation::{ActivationEvent, EventSource, Scope, Severity};
use tuning_agent::config::Config;
use tuning_agent::runtime::Runtime;

fn main() {
    let args = parse_args();
    let config = match Config::load(args.config_path.as_deref()) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("config error: {err}");
            std::process::exit(1);
        }
    };

    match args.command {
        Command::Daemon => {
            let mut runtime = match Runtime::new(config) {
                Ok(runtime) => runtime,
                Err(err) => {
                    eprintln!("runtime initialization error: {err}");
                    std::process::exit(1);
                }
            };
            if let Err(err) = runtime.run_daemon() {
                eprintln!("tuning-agent error: {err}");
                std::process::exit(1);
            }
        }
        Command::Activate { event } => {
            if let Err(err) = send_unix_activation(&config.activation.socket_path, &event) {
                eprintln!("activation send failed: {err}");
                std::process::exit(1);
            }
            println!("activation sent: {}", event.event_type);
        }
        Command::Help => print_help(),
    }
}

struct Args {
    config_path: Option<PathBuf>,
    command: Command,
}

enum Command {
    Daemon,
    Activate { event: ActivationEvent },
    Help,
}

fn parse_args() -> Args {
    let mut args = std::env::args().skip(1);
    let mut config_path = None;

    let first = args.next();
    let command = match first.as_deref() {
        Some("--config") | Some("-c") => {
            config_path = args.next().map(PathBuf::from);
            parse_command(args.next().as_deref(), &mut args)
        }
        other => parse_command(other, &mut args),
    };

    Args {
        config_path,
        command,
    }
}

fn parse_command(command: Option<&str>, args: &mut impl Iterator<Item = String>) -> Command {
    match command {
        Some("daemon") => Command::Daemon,
        Some("activate") => {
            let event_type = args.next().unwrap_or_else(|| "manual".to_string());
            let severity = args
                .next()
                .as_deref()
                .map(parse_severity)
                .unwrap_or(Severity::Info);
            let source = args
                .next()
                .as_deref()
                .map(parse_source)
                .unwrap_or(EventSource::Cli);
            let scope = args.next().map(Scope::Cgroup).unwrap_or(Scope::Host);
            Command::Activate {
                event: ActivationEvent::new(source, event_type, severity, scope),
            }
        }
        Some("-h") | Some("--help") | None => Command::Help,
        Some(_) => Command::Help,
    }
}

fn parse_severity(input: &str) -> Severity {
    match input {
        "warning" | "warn" => Severity::Warning,
        "critical" | "crit" => Severity::Critical,
        _ => Severity::Info,
    }
}

fn parse_source(input: &str) -> EventSource {
    match input {
        "internal" => EventSource::Internal,
        "cli" => EventSource::Cli,
        name => EventSource::Program(name.to_string()),
    }
}

fn print_help() {
    println!("usage:");
    println!("  tuning-agent [--config path] daemon");
    println!(
        "  tuning-agent [--config path] activate [event_type] [info|warning|critical] [cli|internal|name] [cgroup_path]"
    );
    println!();
    println!("behavior:");
    println!("  daemon listens for Unix IPC and configured timer activation events.");
    println!("  activate sends one ActivationEvent to the daemon over Unix IPC.");
}
