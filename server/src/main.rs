mod convert;
mod service;

pub mod proto {
    tonic::include_proto!("thumbd.v1");
}

use std::time::Duration;

use clap::Parser;
use proto::image_converter_server::ImageConverterServer;
use service::ImageConverterService;
use tonic::transport::Server;
use tracing::info;

/// Graceful shutdown drain timeout (spec §6.4).
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(target_os = "macos")]
const DEFAULT_ADDR: &str = "unix:/tmp/thumbd/thumbd.sock";
#[cfg(not(target_os = "macos"))]
const DEFAULT_ADDR: &str = "unix:/run/thumbd/thumbd.sock";

#[derive(Parser)]
#[command(name = "thumbd", about = "thumbd image conversion server", version)]
struct Args {
    /// Bind address. TCP: [::]:50051  /  UDS: unix:/run/thumbd/thumbd.sock (Linux) or unix:/tmp/thumbd/thumbd.sock (macOS)
    #[arg(short, long, default_value = DEFAULT_ADDR)]
    addr: String,

    /// Number of workers (default: CPU cores - 1, minimum 1)
    #[arg(short, long)]
    workers: Option<usize>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    let n_workers = args.workers.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get().saturating_sub(1).max(1))
            .unwrap_or(1)
    });

    let svc = ImageConverterService::new(n_workers);
    let drain_handle = svc.shutdown_handle();

    let (mut health_reporter, health_service) = tonic_health::server::health_reporter();
    health_reporter
        .set_serving::<ImageConverterServer<ImageConverterService>>()
        .await;

    // Spec §2.2: keepalive parameters.
    // Spec §11: max_receive_message_size = 50 MiB (matches MAX_IMAGE_BYTES).
    let builder = Server::builder()
        .http2_keepalive_interval(Some(Duration::from_secs(30)))
        .http2_keepalive_timeout(Some(Duration::from_secs(5)))
        .add_service(health_service)
        .add_service(ImageConverterServer::new(svc).max_decoding_message_size(50 * 1024 * 1024));

    if let Some(uds_path) = args.addr.strip_prefix("unix:") {
        use std::os::unix::fs::PermissionsExt;
        use tokio::net::UnixListener;
        use tokio_stream::wrappers::UnixListenerStream;

        // Ensure parent directory exists, then remove any stale socket before binding
        if let Some(parent) = std::path::Path::new(uds_path).parent() {
            std::fs::create_dir_all(parent)?;
        }
        let _ = std::fs::remove_file(uds_path);
        let listener = UnixListener::bind(uds_path)?;
        // Restrict socket to owner+group only (rw-rw----).
        // TCP TLS is required for cross-host connections (see spec §11); configure via
        // a TLS-terminating proxy or supply tonic::transport::ServerTlsConfig.
        std::fs::set_permissions(uds_path, std::fs::Permissions::from_mode(0o660))?;
        let incoming = UnixListenerStream::new(listener);

        info!(addr = args.addr, n_workers, "starting thumbd (UDS)");

        builder
            .serve_with_incoming_shutdown(incoming, shutdown_signal(drain_handle))
            .await?;
    } else {
        let addr = args.addr.parse()?;

        info!(
            addr = %addr,
            n_workers, "starting thumbd (TCP)"
        );

        builder
            .serve_with_shutdown(addr, shutdown_signal(drain_handle))
            .await?;
    }

    Ok(())
}

/// Spec §6.4: on SIGINT, close semaphores (queued requests get UNAVAILABLE),
/// then wait for in-flight conversions to finish (up to DRAIN_TIMEOUT).
async fn shutdown_signal(drain_handle: service::ShutdownHandle) {
    tokio::signal::ctrl_c().await.ok();
    info!("shutdown signal received, draining...");
    drain_handle.initiate();
    drain_handle.drain(DRAIN_TIMEOUT).await;
    info!("drain complete");
}
