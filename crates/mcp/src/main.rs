//! `colab-playground-mcp`: the MCP server from `docs/mcp-design.md` — the
//! GUI's core, driven by a local agent over stdio instead of by the GUI.
//!
//! The server binds nothing and opens no ports; it serves only the process
//! that spawned it. With the real backend it shares the app's data
//! directory (accounts, ledger, plugins), so state is continuous with the
//! GUI, and it takes the driver lock there first (see `lock`).

mod lock;
mod server;

use std::{process::ExitCode, sync::Arc, time::Duration};

use colab_playground_core::{
    Defaults, SystemClock,
    backend::Backend,
    mock::{MockBackend, MockConfig, MockFailures, MockScenario},
    paths,
    real::{RealBackend, RealConfig},
};
use rmcp::{service::ServiceExt, transport::stdio};

use lock::DriverLock;
use server::McpServer;

/// Environment variable that picks the backend when no flag is given
/// (same variable and values as the GUI).
const BACKEND_ENV: &str = "COLAB_PLAYGROUND_BACKEND";

const USAGE: &str = "\
usage: colab-playground-mcp [--real | --mock]
  --real   drive the real colab CLI (default in release builds)
  --mock   deterministic simulation; never touches the CLI or an account
           (default in debug builds)
  The COLAB_PLAYGROUND_BACKEND environment variable (mock|real) is the
  fallback when no flag is given.";

#[derive(Clone, Copy, PartialEq, Eq)]
enum BackendChoice {
    Mock,
    Real,
}

fn resolve_choice(args: &[String]) -> Result<BackendChoice, String> {
    let mock = args.iter().any(|a| a == "--mock");
    let real = args.iter().any(|a| a == "--real");
    match (mock, real) {
        (true, true) => return Err("pass at most one of --mock and --real".into()),
        (true, false) => return Ok(BackendChoice::Mock),
        (false, true) => return Ok(BackendChoice::Real),
        (false, false) => {}
    }
    match std::env::var(BACKEND_ENV) {
        Ok(value) if value.eq_ignore_ascii_case("mock") => Ok(BackendChoice::Mock),
        Ok(value) if value.eq_ignore_ascii_case("real") => Ok(BackendChoice::Real),
        Ok(value) if value.trim().is_empty() => Ok(build_default()),
        Ok(value) => Err(format!(
            "{BACKEND_ENV}={value:?} is not one of \"mock\" or \"real\""
        )),
        Err(_) => Ok(build_default()),
    }
}

/// Debug builds default to the simulation, release builds to the real CLI —
/// the DESIGN hard safety rule the GUI follows too.
fn build_default() -> BackendChoice {
    if cfg!(debug_assertions) {
        BackendChoice::Mock
    } else {
        BackendChoice::Real
    }
}

fn mock_backend() -> Arc<dyn Backend> {
    Arc::new(MockBackend::new(MockConfig {
        clock: Arc::new(SystemClock),
        step_delay: Duration::ZERO,
        failures: MockFailures::default(),
        scenario: MockScenario::demo(),
        defaults: Defaults::default(),
    }))
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    if let Some(unknown) = args
        .iter()
        .find(|a| !matches!(a.as_str(), "--mock" | "--real"))
    {
        eprintln!("unknown argument {unknown:?}\n{USAGE}");
        return ExitCode::from(2);
    }
    let choice = match resolve_choice(&args) {
        Ok(choice) => choice,
        Err(message) => {
            eprintln!("{message}\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    // The lock is held for the whole serve: keep it bound until main returns.
    let mut _lock: Option<DriverLock> = None;
    let (backend, label, data_dir): (Arc<dyn Backend>, &'static str, Option<_>) = match choice {
        BackendChoice::Mock => (mock_backend(), "mock", None),
        BackendChoice::Real => {
            let Some(data_dir) = paths::default_local_data_dir() else {
                eprintln!("no data directory on this system");
                return ExitCode::FAILURE;
            };
            match DriverLock::acquire(&data_dir) {
                Ok(lock) => _lock = Some(lock),
                Err(e) => {
                    eprintln!("{e}");
                    return ExitCode::FAILURE;
                }
            }
            let config = match RealConfig::for_app(Defaults::default()) {
                Ok(config) => config,
                Err(e) => {
                    eprintln!("backend setup failed: {}", e.detail);
                    return ExitCode::FAILURE;
                }
            };
            match RealBackend::new(config) {
                Ok(backend) => (Arc::new(backend), "real", Some(data_dir)),
                Err(e) => {
                    eprintln!("backend setup failed: {}", e.detail);
                    return ExitCode::FAILURE;
                }
            }
        }
    };

    let server = McpServer::new(backend, label, data_dir);
    let service = match server.serve(stdio()).await {
        Ok(service) => service,
        Err(e) => {
            eprintln!("serve failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    match service.waiting().await {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("server stopped with an error: {e}");
            ExitCode::FAILURE
        }
    }
}
