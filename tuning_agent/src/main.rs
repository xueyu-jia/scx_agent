use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tuning_agent::activation::source::{send_unix_activation, send_unix_activation_request};
use tuning_agent::activation::{ActivationEvent, ActivationRequest, EventSource, Scope, Severity};
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
        Command::Activate {
            event,
            wait,
            json,
            timeout,
        } => {
            if wait {
                let request = ActivationRequest::new(new_request_id(), true, event);
                let response = match send_unix_activation_request(
                    &config.activation.socket_path,
                    &request,
                    timeout,
                ) {
                    Ok(response) => response,
                    Err(err) => {
                        eprintln!("activation wait failed: {err}");
                        std::process::exit(1);
                    }
                };
                if json {
                    match serde_json::to_string(&response) {
                        Ok(encoded) => println!("{encoded}"),
                        Err(err) => {
                            eprintln!("activation response encode failed: {err}");
                            std::process::exit(1);
                        }
                    }
                } else {
                    println!(
                        "activation finished: request_id={} status={:?}",
                        response.request_id, response.status
                    );
                }
            } else {
                if let Err(err) = send_unix_activation(&config.activation.socket_path, &event) {
                    eprintln!("activation send failed: {err}");
                    std::process::exit(1);
                }
                if json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "version": 1,
                            "request_id": null,
                            "status": "sent",
                            "accepted": null,
                        })
                    );
                } else {
                    println!("activation sent: {}", event.event_type);
                }
            }
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
    Activate {
        event: ActivationEvent,
        wait: bool,
        json: bool,
        timeout: Duration,
    },
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
            let mut wait = false;
            let mut json = false;
            let mut timeout = Duration::from_secs(600);
            let mut positional = Vec::new();
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--wait" => wait = true,
                    "--json" => json = true,
                    "--timeout-seconds" => {
                        let Some(value) = args.next() else {
                            return Command::Help;
                        };
                        timeout = match value.parse::<u64>() {
                            Ok(seconds) if seconds > 0 => Duration::from_secs(seconds),
                            _ => return Command::Help,
                        };
                    }
                    _ => positional.push(arg),
                }
            }
            let event_type = positional
                .first()
                .cloned()
                .unwrap_or_else(|| "manual".to_string());
            let severity = positional
                .get(1)
                .map(String::as_str)
                .map(parse_severity)
                .unwrap_or(Severity::Info);
            let source = positional
                .get(2)
                .map(String::as_str)
                .map(parse_source)
                .unwrap_or(EventSource::Cli);
            let scope = positional
                .get(3)
                .cloned()
                .map(Scope::Cgroup)
                .unwrap_or(Scope::Host);
            Command::Activate {
                event: ActivationEvent::new(source, event_type, severity, scope),
                wait,
                json,
                timeout,
            }
        }
        Some("-h") | Some("--help") | None => Command::Help,
        Some(_) => Command::Help,
    }
}

fn new_request_id() -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("cli-{}-{timestamp}", std::process::id())
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
        "  tuning-agent [--config path] activate [--wait] [--json] [--timeout-seconds N] [event_type] [info|warning|critical] [cli|internal|name] [cgroup_path]"
    );
    println!();
    println!("behavior:");
    println!("  daemon listens for Unix IPC and configured timer activation events.");
    println!("  activate sends one ActivationEvent to the daemon over Unix IPC.");
    println!("  activate --wait --json prints a structured ActivationResponse.");
}
