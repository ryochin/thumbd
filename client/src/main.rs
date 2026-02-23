use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use tonic::transport::Channel;
use tonic::Request;

pub mod proto {
    tonic::include_proto!("thumbd.v1");
}

use proto::{image_converter_client::ImageConverterClient, ConvertRequest};

#[cfg(target_os = "macos")]
const DEFAULT_SERVER: &str = "unix:/tmp/thumbd/thumbd.sock";
#[cfg(not(target_os = "macos"))]
const DEFAULT_SERVER: &str = "unix:/run/thumbd/thumbd.sock";

#[derive(Parser, Debug)]
#[command(name = "thumbd-client", about = "thumbd test client", version)]
struct Args {
    /// Source image file path
    input: PathBuf,

    /// Server address (e.g. unix:/run/thumbd/thumbd.sock or http://localhost:50051)
    #[arg(short, long, default_value = DEFAULT_SERVER)]
    server: String,

    /// Maximum output width in pixels
    #[arg(short = 'W', long, default_value_t = 320)]
    max_width: u32,

    /// Maximum output height in pixels
    #[arg(short = 'H', long, default_value_t = 240)]
    max_height: u32,

    /// WebP quality (1-100)
    #[arg(short, long, default_value_t = 80)]
    quality: u32,

    /// WebP encoding effort (1-6; higher = better compression, slower)
    #[arg(short, long, default_value_t = 4)]
    effort: u32,

    /// Request deadline in milliseconds
    #[arg(short, long, default_value_t = 5000)]
    deadline: u64,

    /// Overwrite output file without prompting (normal mode only)
    #[arg(long)]
    force: bool,

    /// Benchmark mode: number of concurrent requests (must be >= 1)
    #[arg(long, value_name = "CONCURRENCY", value_parser = parse_nonzero_usize)]
    bench: Option<usize>,

    /// Total number of requests in benchmark mode (default: same as --bench, must be >= 1)
    #[arg(long, value_name = "N", value_parser = parse_nonzero_usize)]
    requests: Option<usize>,
}

// ---------- clap バリデータ ----------

fn parse_nonzero_usize(s: &str) -> Result<usize, String> {
    let n: usize = s
        .parse()
        .map_err(|e: std::num::ParseIntError| e.to_string())?;
    if n == 0 {
        Err("must be >= 1".to_string())
    } else {
        Ok(n)
    }
}

// ---------- ユーティリティ ----------

fn output_path(input: &Path) -> PathBuf {
    input.with_extension("webp")
}

fn check_overwrite_needed(path: &Path, force: bool) -> bool {
    path.exists() && !force
}

fn prompt_overwrite(path: &Path) -> bool {
    eprint!("{} already exists. Overwrite? [y/N] ", path.display());
    let mut input = String::new();
    std::io::stdin().read_line(&mut input).unwrap_or(0);
    input.trim().eq_ignore_ascii_case("y")
}

async fn connect(addr: &str) -> Result<Channel, Box<dyn std::error::Error>> {
    if let Some(path) = addr.strip_prefix("unix:") {
        use hyper_util::rt::TokioIo;
        use tokio::net::UnixStream;
        use tonic::transport::{Endpoint, Uri};
        use tower::service_fn;
        let path = path.to_owned();
        let channel = Endpoint::try_from("http://[::]:0")?
            .connect_with_connector(service_fn(move |_: Uri| {
                let path = path.clone();
                async move { UnixStream::connect(&path).await.map(TokioIo::new) }
            }))
            .await?;
        Ok(channel)
    } else {
        let channel = Channel::from_shared(addr.to_owned())?.connect().await?;
        Ok(channel)
    }
}

fn make_request(image_data: Vec<u8>, args: &Args, deadline: u64) -> Request<ConvertRequest> {
    let mut req = Request::new(ConvertRequest {
        image_data,
        max_width: args.max_width,
        max_height: args.max_height,
        quality: args.quality,
        effort: args.effort,
    });
    req.set_timeout(Duration::from_millis(deadline));
    req
}

// ---------- ベンチマークモード ----------

struct BenchResult {
    elapsed_ms: u64,
    work_ms: u32, // サーバ側変換時間
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 * p / 100.0).ceil() as usize).saturating_sub(1);
    sorted[idx.min(sorted.len() - 1)]
}

async fn run_bench(channel: Channel, image_data: Arc<Vec<u8>>, args: &Args) {
    let concurrency = args.bench.unwrap();
    let total = args.requests.unwrap_or(concurrency);

    println!(
        "benchmarking: {} requests, concurrency={}",
        total, concurrency
    );

    let sem = Arc::new(tokio::sync::Semaphore::new(concurrency));
    let start = Instant::now();

    let mut handles = Vec::with_capacity(total);
    for _ in 0..total {
        let permit = sem.clone().acquire_owned().await.unwrap();
        let channel = channel.clone();
        let data = Arc::clone(&image_data);
        let deadline = args.deadline;
        let max_width = args.max_width;
        let max_height = args.max_height;
        let quality = args.quality;
        let effort = args.effort;

        handles.push(tokio::spawn(async move {
            let _permit = permit;
            let mut client = ImageConverterClient::new(channel);
            let mut req = Request::new(ConvertRequest {
                image_data: data.as_ref().clone(),
                max_width,
                max_height,
                quality,
                effort,
            });
            req.set_timeout(Duration::from_millis(deadline));

            let t = Instant::now();
            let result = client.convert(req).await;
            let elapsed_ms = t.elapsed().as_millis() as u64;

            match result {
                Ok(res) => Ok(BenchResult {
                    elapsed_ms,
                    work_ms: res.into_inner().work_ms,
                }),
                Err(e) => Err((elapsed_ms, e.message().to_owned())),
            }
        }));
    }

    let mut ok_results: Vec<BenchResult> = Vec::new();
    let mut errors: Vec<(u64, String)> = Vec::new();

    for h in handles {
        match h.await.unwrap() {
            Ok(r) => ok_results.push(r),
            Err(e) => errors.push(e),
        }
    }

    let wall_ms = start.elapsed().as_millis() as u64;
    let n_ok = ok_results.len();
    let n_err = errors.len();

    // クライアント側レイテンシ
    let mut client_ms: Vec<u64> = ok_results.iter().map(|r| r.elapsed_ms).collect();
    client_ms.sort_unstable();

    // サーバ側 work_ms
    let mut server_ms: Vec<u64> = ok_results.iter().map(|r| r.work_ms as u64).collect();
    server_ms.sort_unstable();

    let rps = if wall_ms > 0 {
        n_ok as f64 / (wall_ms as f64 / 1000.0)
    } else {
        f64::INFINITY
    };

    println!(
        "  ok:      {:4}  ({:.1}%)",
        n_ok,
        100.0 * n_ok as f64 / total as f64
    );
    println!(
        "  err:     {:4}  ({:.1}%)",
        n_err,
        100.0 * n_err as f64 / total as f64
    );
    println!("  elapsed: {}ms", wall_ms);
    println!("  rps:     {:.1}", rps);

    if !client_ms.is_empty() {
        println!("  latency (client-side, ok only):");
        println!(
            "    min={:4}ms  p50={:4}ms  p95={:4}ms  p99={:4}ms  max={:4}ms",
            client_ms[0],
            percentile(&client_ms, 50.0),
            percentile(&client_ms, 95.0),
            percentile(&client_ms, 99.0),
            client_ms[client_ms.len() - 1],
        );
        println!("  work_ms (server-side, ok only):");
        println!(
            "    min={:4}ms  p50={:4}ms  p95={:4}ms  p99={:4}ms  max={:4}ms",
            server_ms[0],
            percentile(&server_ms, 50.0),
            percentile(&server_ms, 95.0),
            percentile(&server_ms, 99.0),
            server_ms[server_ms.len() - 1],
        );
    }

    if !errors.is_empty() {
        println!("  errors:");
        let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for (_, msg) in &errors {
            *counts.entry(msg.as_str()).or_insert(0) += 1;
        }
        let mut pairs: Vec<_> = counts.into_iter().collect();
        pairs.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        for (msg, n) in pairs {
            println!("    {}x  {}", n, msg);
        }
    }
}

// ---------- main ----------

#[tokio::main]
async fn main() {
    let args = Args::parse();

    if !args.input.exists() {
        eprintln!("error: input file not found: {}", args.input.display());
        std::process::exit(1);
    }

    let image_data = match std::fs::read(&args.input) {
        Ok(data) => data,
        Err(e) => {
            eprintln!("error: failed to read input: {e}");
            std::process::exit(1);
        }
    };

    let channel = match connect(&args.server).await {
        Ok(ch) => ch,
        Err(e) => {
            eprintln!("error: failed to connect to {}: {e}", args.server);
            std::process::exit(1);
        }
    };

    // ベンチマークモード
    if args.bench.is_some() {
        run_bench(channel, Arc::new(image_data), &args).await;
        return;
    }

    // 通常変換モード
    let out_path = output_path(&args.input);

    if check_overwrite_needed(&out_path, args.force) && !prompt_overwrite(&out_path) {
        eprintln!("aborted");
        std::process::exit(1);
    }

    let mut client = ImageConverterClient::new(channel);
    let request = make_request(image_data, &args, args.deadline);

    let response = match client.convert(request).await {
        Ok(res) => res.into_inner(),
        Err(e) => {
            eprintln!("error: {}", e.message());
            std::process::exit(1);
        }
    };

    if let Err(e) = std::fs::write(&out_path, &response.output_data) {
        eprintln!("error: failed to write output: {e}");
        std::process::exit(1);
    }

    println!(
        "{} ({}x{}, {}ms)",
        out_path.display(),
        response.width,
        response.height,
        response.work_ms,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_path_replaces_extension() {
        assert_eq!(
            output_path(Path::new("photo.jpg")),
            PathBuf::from("photo.webp")
        );
        assert_eq!(
            output_path(Path::new("image.png")),
            PathBuf::from("image.webp")
        );
        assert_eq!(
            output_path(Path::new("a/b/c.jpeg")),
            PathBuf::from("a/b/c.webp")
        );
    }

    #[test]
    fn output_path_no_extension() {
        assert_eq!(output_path(Path::new("noext")), PathBuf::from("noext.webp"));
    }

    #[test]
    fn output_path_already_webp() {
        assert_eq!(
            output_path(Path::new("photo.webp")),
            PathBuf::from("photo.webp")
        );
    }

    #[test]
    fn check_overwrite_file_not_exists() {
        let path = Path::new("/tmp/definitely_does_not_exist_xyzxyz.webp");
        assert!(!check_overwrite_needed(path, false));
        assert!(!check_overwrite_needed(path, true));
    }

    #[test]
    fn check_overwrite_force() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        assert!(!check_overwrite_needed(tmp.path(), true));
    }

    #[test]
    fn check_overwrite_needed_when_exists() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        assert!(check_overwrite_needed(tmp.path(), false));
    }

    #[test]
    fn args_required_fields() {
        let args = Args::try_parse_from([
            "client",
            "photo.jpg",
            "--max-width",
            "320",
            "--max-height",
            "240",
        ])
        .unwrap();
        assert_eq!(args.max_width, 320);
        assert_eq!(args.max_height, 240);
        assert_eq!(args.quality, 80);
        assert_eq!(args.effort, 4);
        assert!(!args.force);
    }

    #[test]
    fn args_defaults() {
        let args = Args::try_parse_from(["client", "photo.jpg"]).unwrap();
        assert_eq!(args.max_width, 320);
        assert_eq!(args.max_height, 240);
        assert_eq!(args.deadline, 5000);
    }

    #[test]
    fn args_force_flag() {
        let args = Args::try_parse_from([
            "client",
            "photo.jpg",
            "--max-width",
            "100",
            "--max-height",
            "100",
            "--force",
        ])
        .unwrap();
        assert!(args.force);
    }

    #[test]
    fn args_bench_mode() {
        let args = Args::try_parse_from([
            "client",
            "photo.jpg",
            "--max-width",
            "320",
            "--max-height",
            "240",
            "--bench",
            "10",
        ])
        .unwrap();
        assert_eq!(args.bench, Some(10));
        assert_eq!(args.requests, None);
    }

    #[test]
    fn args_bench_with_requests() {
        let args = Args::try_parse_from([
            "client",
            "photo.jpg",
            "--max-width",
            "320",
            "--max-height",
            "240",
            "--bench",
            "10",
            "--requests",
            "100",
        ])
        .unwrap();
        assert_eq!(args.bench, Some(10));
        assert_eq!(args.requests, Some(100));
    }

    #[test]
    fn percentile_basic() {
        let data = vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        assert_eq!(percentile(&data, 50.0), 5);
        assert_eq!(percentile(&data, 95.0), 10);
        assert_eq!(percentile(&data, 99.0), 10);
    }

    #[test]
    fn percentile_empty() {
        assert_eq!(percentile(&[], 50.0), 0);
    }
}
