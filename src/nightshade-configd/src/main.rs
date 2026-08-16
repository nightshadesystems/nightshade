//! The Nightshade configuration daemon.

use std::process::ExitCode;
use std::sync::Arc;

use nightshade_common::{VERSION, paths::Paths};
use nightshade_configd::{Access, Bound, Configd, Server, logging};
use nightshade_render::RealHost;
use nightshade_schema::model::Schema;
use tokio::sync::watch;
use tracing::{error, info};

fn main() -> ExitCode {
    logging::init();

    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // Both, deliberately. The journal is where this belongs, and
            // stderr is where someone running it by hand to find out why it
            // will not start is looking.
            error!(error = %e, "configd could not start");
            eprintln!("nightshade-configd: {e}");
            ExitCode::FAILURE
        }
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let paths = Paths::system();
    let access = Access::default();
    let schema = Schema::compiled();

    info!(version = VERSION, "starting");

    let configd = Arc::new(Configd::start(schema, paths.clone(), Arc::new(RealHost))?);

    // Before the socket opens. If a commit was left waiting on confirmation
    // when configd stopped, it is either resumed or rolled back now -- not
    // after the first client happens to connect.
    configd.resume().await;

    // Apply the saved configuration. After `resume`, so a commit that was
    // waiting on confirmation is settled before anything else touches the box.
    let outcome = configd.boot().await;
    match outcome.failed() {
        // On the console as well as in the journal. This is the one startup
        // result somebody has to see, and they are about to log in to a box
        // that is not configured the way they left it.
        Some(reason) => eprintln!(
            "nightshade-configd: started with DEFAULT configuration only.\n{reason}"
        ),
        None => info!(?outcome, "startup"),
    }

    // systemd's socket if there is one, ours if not. Both give the same
    // socket with the same mode; the difference is only who created it.
    let bound = match Bound::from_systemd()? {
        Some(bound) => bound,
        None => Bound::create(&paths.socket(), &access)?,
    };

    let (tx, rx) = watch::channel(false);
    tokio::spawn(async move {
        wait_for_signal().await;
        let _ = tx.send(true);
    });

    Server::new(configd, access).run(bound, rx).await;
    info!("stopped");
    Ok(())
}

/// SIGTERM is what systemd sends; SIGINT is what a person running it in a
/// terminal sends. Both mean stop, and both should mean stop *cleanly* -- a
/// config daemon killed mid-apply is the situation the whole rollback
/// machinery exists to avoid entering by accident.
async fn wait_for_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let mut term = match signal(SignalKind::terminate()) {
        Ok(signal) => signal,
        Err(e) => {
            error!(error = %e, "cannot listen for SIGTERM");
            return;
        }
    };
    let mut interrupt = match signal(SignalKind::interrupt()) {
        Ok(signal) => signal,
        Err(e) => {
            error!(error = %e, "cannot listen for SIGINT");
            return;
        }
    };

    tokio::select! {
        _ = term.recv() => info!("SIGTERM"),
        _ = interrupt.recv() => info!("SIGINT"),
    }
}
