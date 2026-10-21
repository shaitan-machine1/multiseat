use clap::{Parser, Subcommand, ValueEnum};
use std::io::{self, Write};
use std::process::ExitCode;

type RefreshDescription = (String, String, Vec<String>, Vec<String>);

#[zbus::proxy(
    interface = "org.decoupling.multiseatd1.Manager",
    default_service = "org.decoupling.multiseatd1",
    default_path = "/org/decoupling/multiseatd1"
)]
trait Manager {
    fn list_seats(&self) -> zbus::Result<Vec<String>>;
    fn preview_refresh(&self, seat: &str) -> zbus::Result<Vec<RefreshDescription>>;
    fn refresh(&self, seat: &str, confirmed: bool) -> zbus::Result<Vec<String>>;
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum BusKind {
    System,
    Session,
}

#[derive(Debug, Parser)]
#[command(version, about = "Inspect and explicitly refresh multiseatd seats")]
struct Args {
    #[arg(long, value_enum, default_value_t = BusKind::System)]
    bus: BusKind,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    List,
    Refresh {
        seat: Option<String>,
        #[arg(long)]
        force: bool,
    },
}

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("multiseatctl: {error}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(Args::parse())) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("multiseatctl: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let connection = match args.bus {
        BusKind::System => zbus::Connection::system().await?,
        BusKind::Session => zbus::Connection::session().await?,
    };
    let proxy = ManagerProxy::new(&connection).await?;
    match args.command {
        Command::List => {
            for seat in proxy.list_seats().await? {
                println!("{seat}");
            }
        }
        Command::Refresh { seat, force } => {
            let scope = seat.unwrap_or_default();
            let changes = proxy.preview_refresh(&scope).await?;
            if changes.is_empty() {
                println!("No seat configuration changes.");
                return Ok(());
            }
            print_changes(&changes);
            print_known_sessions(&connection).await;
            eprintln!(
                "WARNING: configured devices are seatd allowlists, not devices currently open by clients."
            );
            eprintln!("WARNING: refreshing these seats may terminate graphical sessions.");
            if !force && !confirm()? {
                return Err("refresh cancelled".into());
            }
            let completed = proxy.refresh(&scope, true).await?;
            for seat in completed {
                println!("refreshed {seat}");
            }
        }
    }
    Ok(())
}

fn print_changes(changes: &[RefreshDescription]) {
    for (seat, action, old, new) in changes {
        println!("{seat}: {action}");
        println!(
            "  old allowlist: {}",
            if old.is_empty() {
                "(empty)".into()
            } else {
                old.join(", ")
            }
        );
        println!(
            "  new allowlist: {}",
            if new.is_empty() {
                "(empty)".into()
            } else {
                new.join(", ")
            }
        );
    }
}

async fn print_known_sessions(connection: &zbus::Connection) {
    let Ok(proxy) = zbus::Proxy::new(
        connection,
        "org.decoupling.sessiond1",
        "/org/decoupling/sessiond1",
        "org.decoupling.sessiond1.Manager",
    )
    .await
    else {
        return;
    };
    let sessions: zbus::Result<Vec<(String, u32)>> = proxy.call("ListSessions", &()).await;
    if let Ok(sessions) = sessions {
        if !sessions.is_empty() {
            eprintln!("Known sessions (sessiond cannot yet map these precisely to seats):");
            for (sid, uid) in sessions {
                eprintln!("  {sid} uid={uid}");
            }
        }
    }
}

fn confirm() -> io::Result<bool> {
    eprint!("Continue? [y/N] ");
    io::stderr().flush()?;
    let mut response = String::new();
    io::stdin().read_line(&mut response)?;
    Ok(matches!(response.trim(), "y" | "Y" | "yes" | "YES"))
}
