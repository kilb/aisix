//! `aisix-control-plane [serve]`: configuration from the environment (see
//! `aisix_control_plane::config`). A refused boot prints one line naming the
//! variable to fix and exits 1.
//!
//! `aisix-control-plane create-operator --email <e> --role <admin|operator|viewer> [--display-name <n>]`
//! creates an operator, with the password read from the first line of
//! stdin; it is how the first admin comes to exist. It exits 1 when it
//! refuses (an invalid email or display name, a password under 12
//! characters, an email already taken) and 2 on a usage error.

use std::io::BufRead;
use std::process::ExitCode;

use aisix_control_plane::auth::OperatorRole;
use aisix_control_plane::config::CpConfig;
use aisix_control_plane::db::Db;
use tracing_subscriber::EnvFilter;

const USAGE: &str = "usage: aisix-control-plane [serve]\n       \
    aisix-control-plane create-operator --email <email> --role <admin|operator|viewer> \
    [--display-name <name>]   (the password is the first line of stdin)";

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args: Option<Vec<String>> = std::env::args_os()
        .skip(1)
        .map(|a| a.into_string().ok())
        .collect();
    let Some(args) = args else {
        return usage();
    };
    match args.split_first() {
        None => serve().await,
        Some((first, rest)) if first == "serve" && rest.is_empty() => serve().await,
        Some((first, rest)) if first == "create-operator" => create_operator(rest).await,
        Some(_) => usage(),
    }
}

fn usage() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::from(2)
}

fn config() -> Result<CpConfig, ExitCode> {
    CpConfig::from_env().map_err(|e| {
        eprintln!("aisix-control-plane: {e}");
        ExitCode::FAILURE
    })
}

async fn serve() -> ExitCode {
    let cfg = match config() {
        Ok(cfg) => cfg,
        Err(code) => return code,
    };
    match aisix_control_plane::run(cfg).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("aisix-control-plane: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The flags of `create-operator`.
struct CreateArgs {
    email: String,
    role: OperatorRole,
    display_name: Option<String>,
}

fn parse_create_args(args: &[String]) -> Option<CreateArgs> {
    let (mut email, mut role, mut display_name) = (None, None, None);
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        let value = rest.next()?.clone();
        let slot = match flag.as_str() {
            "--email" => &mut email,
            "--role" => &mut role,
            "--display-name" => &mut display_name,
            _ => return None,
        };
        if slot.replace(value).is_some() {
            return None;
        }
    }
    Some(CreateArgs {
        email: email?,
        role: role?.parse().ok()?,
        display_name,
    })
}

async fn create_operator(args: &[String]) -> ExitCode {
    let Some(args) = parse_create_args(args) else {
        return usage();
    };
    let cfg = match config() {
        Ok(cfg) => cfg,
        Err(code) => return code,
    };
    let mut line = String::new();
    if let Err(e) = std::io::stdin().lock().read_line(&mut line) {
        eprintln!("aisix-control-plane: cannot read the password from stdin: {e}");
        return ExitCode::FAILURE;
    }
    let password = line.strip_suffix('\n').unwrap_or(&line);
    let password = password.strip_suffix('\r').unwrap_or(password);
    let db = match Db::connect(
        &cfg.database_url,
        cfg.db_lock_timeout_ms,
        cfg.db_statement_timeout_ms,
    )
    .await
    {
        Ok(db) => db,
        Err(e) => {
            eprintln!("aisix-control-plane: database (CP_DATABASE_URL): {e}");
            return ExitCode::FAILURE;
        }
    };
    match aisix_control_plane::operators::create_first(
        &db,
        &args.email,
        args.role,
        args.display_name.as_deref(),
        password,
    )
    .await
    {
        Ok(op) => {
            println!(
                "created operator {} ({}), role {}",
                op.email, op.id, op.role
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("aisix-control-plane: {e}");
            ExitCode::FAILURE
        }
    }
}
