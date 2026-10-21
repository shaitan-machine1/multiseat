use clap::{Parser, ValueEnum};
use multiseatd::dbus::{BUS_NAME, Manager, OBJECT_PATH, SeatRegistry};
use multiseatd::orchestrator::Orchestrator;
use multiseatd::service::S6SeatServiceManager;
use multiseatd::topology::UdevTopology;
use seat_udev::SeatName;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Clone, Copy, Debug, ValueEnum)]
enum BusKind {
    System,
    Session,
}

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Orchestrate one s6-supervised seatd process per udev seat"
)]
struct Args {
    #[arg(long, default_value = "/run/multiseatd")]
    runtime_dir: PathBuf,
    #[arg(long, default_value = "/usr/bin/seatd")]
    seatd_program: PathBuf,
    #[arg(long, default_value_t = 10_000)]
    readiness_timeout_ms: u64,
    #[arg(long, value_enum, default_value_t = BusKind::System)]
    bus: BusKind,
    /// Add an empty non-VT seat independently of udev (local testing only).
    #[arg(long, hide = true)]
    test_seat: Vec<SeatName>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "multiseatd=info".into()),
        )
        .init();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run(Args::parse()))
}

async fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let topology = UdevTopology::new(args.runtime_dir.clone(), args.test_seat);
    let services = S6SeatServiceManager::new(
        args.runtime_dir,
        args.seatd_program,
        args.readiness_timeout_ms,
    )?;
    let orchestrator = Arc::new(Mutex::new(Orchestrator::new(
        Box::new(topology),
        Box::new(services),
    )));

    let registry = SeatRegistry::default();
    {
        let orchestrator = orchestrator.lock().await;
        for failure in orchestrator.converge_startup()? {
            tracing::error!(%failure, "could not converge seat during startup");
        }
        for seat in orchestrator.ready_seats()? {
            tracing::info!(seat = %seat, "publishing ready seat");
            registry.insert(&seat);
        }
    }

    let builder = match args.bus {
        BusKind::System => zbus::connection::Builder::system(),
        BusKind::Session => zbus::connection::Builder::session(),
    };
    let connection = builder.build().await?;
    connection
        .object_server()
        .at(
            OBJECT_PATH,
            Manager::new(registry.clone(), orchestrator.clone()),
        )
        .await?;
    match connection
        .request_name_with_flags(BUS_NAME, zbus::fdo::RequestNameFlags::DO_NOT_QUEUE)
        .await
    {
        Ok(_) => {}
        Err(zbus::Error::NameTaken) => {
            return Err(format!(
                "{BUS_NAME} is already owned on this bus; another multiseatd is running"
            )
            .into());
        }
        Err(error) => return Err(error.into()),
    }

    tokio::select! {
        result = tokio::signal::ctrl_c() => result?,
        _ = connection.closed() => return Err("lost the D-Bus connection".into()),
    }
    Ok(())
}
