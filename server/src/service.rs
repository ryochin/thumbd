use std::sync::{
    atomic::{AtomicI64, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use tokio::sync::{Semaphore, TryAcquireError};
use tonic::metadata::MetadataMap;
use tonic::{Request, Response, Status};

use thousands::Separable;

use crate::convert::{self, ConvertError, ConvertParams};
use crate::proto::{image_converter_server::ImageConverter, ConvertRequest, ConvertResponse};

const MIN_DEADLINE_MS: u64 = 200;
const MAX_IMAGE_BYTES: usize = 50 * 1024 * 1024;

pub struct ImageConverterService {
    queue_sem: Arc<Semaphore>,
    work_sem: Arc<Semaphore>,
    queue_depth: Arc<AtomicI64>,
    inflight: Arc<AtomicI64>,
}

/// Handle for initiating graceful shutdown and waiting for in-flight work to drain.
pub struct ShutdownHandle {
    queue_sem: Arc<Semaphore>,
    work_sem: Arc<Semaphore>,
    inflight: Arc<AtomicI64>,
}

impl ShutdownHandle {
    /// Close both semaphores so queued/waiting requests return UNAVAILABLE immediately.
    pub fn initiate(&self) {
        self.queue_sem.close();
        self.work_sem.close();
    }

    /// Wait until all in-flight conversions finish, or until `timeout` elapses.
    pub async fn drain(&self, timeout: Duration) {
        let _ = tokio::time::timeout(timeout, async {
            loop {
                if self.inflight.load(Ordering::Acquire) == 0 {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
    }
}

impl ImageConverterService {
    pub fn new(n_workers: usize, queue_capacity: usize) -> Self {
        Self {
            queue_sem: Arc::new(Semaphore::new(queue_capacity)),
            work_sem: Arc::new(Semaphore::new(n_workers)),
            queue_depth: Arc::new(AtomicI64::new(0)),
            inflight: Arc::new(AtomicI64::new(0)),
        }
    }

    pub fn shutdown_handle(&self) -> ShutdownHandle {
        ShutdownHandle {
            queue_sem: Arc::clone(&self.queue_sem),
            work_sem: Arc::clone(&self.work_sem),
            inflight: Arc::clone(&self.inflight),
        }
    }
}

#[tonic::async_trait]
impl ImageConverter for ImageConverterService {
    async fn convert(
        &self,
        request: Request<ConvertRequest>,
    ) -> Result<Response<ConvertResponse>, Status> {
        let start = Instant::now();

        // Spec §6.3 order: field validation → deadline check → admission control
        // Destructure before consuming so we can access both body and metadata.
        let (metadata, _, req) = request.into_parts();
        validate(&req).inspect_err(|e| tracing::error!(message = e.message(), "validate"))?;
        let remaining_ms = check_deadline(&metadata)
            .inspect_err(|e| tracing::error!(message = e.message(), "check_deadline"))?;

        // Admission control: try to enter queue (non-blocking)
        let queue_permit = match self.queue_sem.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(TryAcquireError::NoPermits) => {
                tracing::error!("queue full");
                return Err(Status::resource_exhausted("queue full"));
            }
            Err(TryAcquireError::Closed) => {
                tracing::error!("server shutting down");
                return Err(Status::unavailable("server shutting down"));
            }
        };

        self.queue_depth.fetch_add(1, Ordering::Relaxed);

        // Wait for a worker slot, respecting the remaining deadline
        let queue_wait_budget = Duration::from_millis(remaining_ms).saturating_sub(start.elapsed());

        let work_permit =
            match tokio::time::timeout(queue_wait_budget, self.work_sem.clone().acquire_owned())
                .await
            {
                Ok(Ok(permit)) => permit,
                Ok(Err(_)) => {
                    // Semaphore closed (server shutting down)
                    self.queue_depth.fetch_sub(1, Ordering::Relaxed);
                    tracing::error!("server shutting down");
                    return Err(Status::unavailable("server shutting down"));
                }
                Err(_) => {
                    // Deadline expired while waiting in queue
                    self.queue_depth.fetch_sub(1, Ordering::Relaxed);
                    tracing::error!("deadline exceeded");
                    return Err(Status::deadline_exceeded("deadline exceeded"));
                }
            };

        // Move from queue to inflight
        drop(queue_permit);
        self.queue_depth.fetch_sub(1, Ordering::Relaxed);
        self.inflight.fetch_add(1, Ordering::Relaxed);

        let inflight = self.inflight.clone();
        let params = ConvertParams {
            image_type: req.image_type,
            max_width: req.max_width,
            max_height: req.max_height,
            quality: req.quality.unwrap_or(80),
            effort: req.effort.unwrap_or(3),
        };
        let image_data = req.image_data;
        let input_bytes = image_data.len();

        let work_start = Instant::now();

        let result = tokio::task::spawn_blocking(move || {
            let r = convert::convert(&image_data, &params);
            drop(work_permit);
            r
        })
        .await
        .map_err(|e| {
            tracing::error!("worker panicked: {e}");
            Status::internal(format!("worker panicked: {e}"))
        })?;

        inflight.fetch_sub(1, Ordering::Relaxed);

        let work_ms = work_start.elapsed().as_millis() as u32;

        match result {
            Ok(c) => {
                let width = c.width.separate_with_commas();
                let height = c.height.separate_with_commas();
                let input_bytes_fmt = input_bytes.separate_with_commas();
                let output_bytes = c.output_data.len().separate_with_commas();
                let work_ms_fmt = work_ms.separate_with_commas();
                tracing::info!(
                    input_bytes = %input_bytes_fmt,
                    width = %width,
                    height = %height,
                    work_ms = %work_ms_fmt,
                    output_bytes = %output_bytes,
                    "converted"
                );
                Ok(Response::new(ConvertResponse {
                    output_data: c.output_data,
                    width: c.width,
                    height: c.height,
                    work_ms,
                }))
            }
            Err(ConvertError::Decode(msg)) => {
                tracing::error!(message = %msg, "decode error");
                Err(Status::internal(msg))
            }
            Err(ConvertError::Encode(msg)) => {
                tracing::error!(message = %msg, "encode error");
                Err(Status::internal(msg))
            }
            Err(ConvertError::UnsupportedFormat(n)) => {
                tracing::error!(image_type = n, "unsupported format");
                Err(Status::invalid_argument(format!(
                    "unsupported image_type: {n}"
                )))
            }
        }
    }
}

/// Parse grpc-timeout header from metadata and return remaining milliseconds.
#[allow(clippy::result_large_err)]
fn check_deadline(metadata: &MetadataMap) -> Result<u64, Status> {
    let timeout_str = metadata
        .get("grpc-timeout")
        .ok_or_else(|| Status::invalid_argument("deadline required"))?
        .to_str()
        .map_err(|_| Status::invalid_argument("deadline required"))?;

    let remaining_ms = parse_grpc_timeout_ms(timeout_str)
        .ok_or_else(|| Status::invalid_argument("deadline required"))?;

    if remaining_ms < MIN_DEADLINE_MS {
        return Err(Status::invalid_argument("deadline too short"));
    }

    Ok(remaining_ms)
}

/// Parse gRPC timeout header value (e.g. "1000m", "1S", "500m").
/// Units: H=hours, M=minutes, S=seconds, m=milliseconds, u=microseconds, n=nanoseconds.
/// Uses checked arithmetic to avoid overflow on extreme values.
fn parse_grpc_timeout_ms(s: &str) -> Option<u64> {
    if s.is_empty() {
        return None;
    }
    let (digits, unit) = s.split_at(s.len() - 1);
    let value: u64 = digits.parse().ok()?;
    let ms = match unit {
        "H" => value.checked_mul(3_600_000)?,
        "M" => value.checked_mul(60_000)?,
        "S" => value.checked_mul(1_000)?,
        "m" => value,
        "u" => value.checked_div(1_000).unwrap_or(0),
        "n" => value.checked_div(1_000_000).unwrap_or(0),
        _ => return None,
    };
    Some(ms)
}

#[allow(clippy::result_large_err)]
fn validate(req: &ConvertRequest) -> Result<(), Status> {
    if req.image_data.is_empty() {
        return Err(Status::invalid_argument("image_data: empty"));
    }
    if req.image_data.len() > MAX_IMAGE_BYTES {
        return Err(Status::invalid_argument(format!(
            "image_data: {} bytes exceeds 50MB",
            req.image_data.len()
        )));
    }
    if req.max_width == 0 || req.max_width > 65535 {
        return Err(Status::invalid_argument(format!(
            "max_width: out of range ({})",
            req.max_width
        )));
    }
    if req.max_height == 0 || req.max_height > 65535 {
        return Err(Status::invalid_argument(format!(
            "max_height: out of range ({})",
            req.max_height
        )));
    }
    if let Some(q) = req.quality.filter(|&q| q == 0 || q > 100) {
        return Err(Status::invalid_argument(format!(
            "quality: out of range ({q})"
        )));
    }
    if let Some(e) = req.effort.filter(|&e| e == 0 || e > 6) {
        return Err(Status::invalid_argument(format!(
            "effort: out of range ({e})"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_timeout_milliseconds() {
        assert_eq!(parse_grpc_timeout_ms("1000m"), Some(1000));
        assert_eq!(parse_grpc_timeout_ms("1S"), Some(1000));
        assert_eq!(parse_grpc_timeout_ms("1M"), Some(60_000));
        assert_eq!(parse_grpc_timeout_ms("1H"), Some(3_600_000));
        assert_eq!(parse_grpc_timeout_ms("500m"), Some(500));
        assert_eq!(parse_grpc_timeout_ms("100u"), Some(0));
    }

    #[test]
    fn parse_timeout_overflow() {
        // u64::MAX * 3_600_000 would overflow without checked_mul
        assert_eq!(parse_grpc_timeout_ms(&format!("{}H", u64::MAX)), None);
        assert_eq!(parse_grpc_timeout_ms(&format!("{}M", u64::MAX)), None);
        assert_eq!(parse_grpc_timeout_ms(&format!("{}S", u64::MAX)), None);
    }

    #[test]
    fn parse_timeout_invalid() {
        assert_eq!(parse_grpc_timeout_ms(""), None);
        assert_eq!(parse_grpc_timeout_ms("abc"), None);
        assert_eq!(parse_grpc_timeout_ms("1X"), None);
    }

    fn make_valid_req() -> ConvertRequest {
        ConvertRequest {
            image_data: vec![0u8; 100],
            image_type: 1,
            max_width: 320,
            max_height: 240,
            quality: None,
            effort: None,
        }
    }

    #[test]
    fn validate_passes_valid_request() {
        assert!(validate(&make_valid_req()).is_ok());
    }

    #[test]
    fn validate_rejects_empty_image() {
        let mut req = make_valid_req();
        req.image_data = vec![];
        assert!(validate(&req).is_err());
    }

    #[test]
    fn validate_rejects_zero_width() {
        let mut req = make_valid_req();
        req.max_width = 0;
        assert!(validate(&req).is_err());
    }

    #[test]
    fn validate_rejects_zero_height() {
        let mut req = make_valid_req();
        req.max_height = 0;
        assert!(validate(&req).is_err());
    }

    #[test]
    fn validate_rejects_quality_over_100() {
        let mut req = make_valid_req();
        req.quality = Some(101);
        assert!(validate(&req).is_err());
    }

    #[test]
    fn validate_rejects_effort_over_6() {
        let mut req = make_valid_req();
        req.effort = Some(7);
        assert!(validate(&req).is_err());
    }
}
