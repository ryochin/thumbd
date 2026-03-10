# thumbd

[![Docker](https://img.shields.io/badge/ghcr.io-ryochin%2Fthumbd-blue?logo=docker)](https://ghcr.io/ryochin/thumbd)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

A fast, lightweight gRPC service that converts images to WebP thumbnails.

## Quick Start

The quickest way to run thumbd is with Docker Compose.
Create a `compose.yml`:

```yaml
services:
  thumbd:
    image: ghcr.io/ryochin/thumbd:latest
    volumes:
      - thumbd-socket:/run/thumbd
    restart: unless-stopped
    environment:
      RUST_LOG: thumbd=info
    healthcheck:
      test: ["CMD", "/usr/local/bin/grpc-health-probe", "-addr=unix:/run/thumbd/thumbd.sock"]

volumes:
  thumbd-socket:
```

```sh
docker compose up -d
```

The socket is available at `/run/thumbd/thumbd.sock` inside the named volume.
Mount the same volume in your application container to connect via Unix Domain Socket.

## Features

- **Input formats**: JPEG, PNG, and any format supported by the [`image`](https://crates.io/crates/image) crate
- **Output**: Lossy WebP with configurable quality and encoding effort
- **Transport**: Unix Domain Socket (recommended) or TCP
- **Protocol**: gRPC (`thumbd.v1.ImageConverter`)
- **Concurrency**: Worker-pool model with admission control and deadline enforcement
- **Health check**: `grpc.health.v1.Health` (always `SERVING`)

## Installation

```sh
docker pull ghcr.io/ryochin/thumbd:latest
```

## Usage

### Server

```sh
# Unix Domain Socket (default)
thumbd

# TCP
thumbd --addr 0.0.0.0:50051
```

**Options**

| Flag              | Default                                                                        | Description                                        |
| ----------------- | ------------------------------------------------------------------------------ | -------------------------------------------------- |
| `-a`, `--addr`    | Linux: `unix:/run/thumbd/thumbd.sock`<br>macOS: `unix:/tmp/thumbd/thumbd.sock` | Bind address — TCP: `host:port`, UDS: `unix:/path` |
| `-w`, `--workers` | CPU cores − 1 (min 1)                                                          | Number of conversion workers                       |

### Client CLI

See [client/README.md](client/README.md) for the `thumbd-client` CLI tool, which also includes a built-in benchmark mode.

## API

### Proto

Service definition ([`proto/thumbd.proto`](proto/thumbd.proto)):

```protobuf
service ImageConverter {
  rpc Convert(ConvertRequest) returns (ConvertResponse);
}

message ConvertRequest {
  bytes           image_data  = 1;  // Raw image bytes (JPEG, PNG, ...)
  ImageType       image_type  = 2;  // Output format: WEBP (UNSPECIFIED also treated as WEBP)
  uint32          max_width   = 3;  // Maximum output width in pixels
  uint32          max_height  = 4;  // Maximum output height in pixels
  optional uint32 quality     = 5;  // WebP quality 1–100 (default: 80)
  optional uint32 effort      = 6;  // WebP encoding effort 1–6 (default: 3)
}

message ConvertResponse {
  bytes  output_data = 1;  // Converted WebP bytes
  uint32 width       = 2;  // Actual output width
  uint32 height      = 3;  // Actual output height
  uint32 work_ms     = 4;  // Server-side processing time in ms
}
```

### Field Constraints

| Field        | Constraint                  |
| ------------ | --------------------------- |
| `image_data` | 1 byte – 50 MiB             |
| `max_width`  | 1 – 65535 px                |
| `max_height` | 1 – 65535 px                |
| `quality`    | 1 – 100 (default: 80)       |
| `effort`     | 1 – 6 (default: 3)          |

> **Server-side input limits:** The source image must not exceed 16384 × 16384 pixels or 512 MiB of decoded memory. Violations return `INTERNAL`.

### Resize Behaviour

Images are only scaled **down** — never upscaled.
If the input already fits within `max_width × max_height`, it is returned at its original size.

```
scale  = min(max_width / W, max_height / H, 1.0)
output = (floor(W × scale), floor(H × scale))
```

### gRPC Status Codes

| Status               | Cause                                                                                |
| -------------------- | ------------------------------------------------------------------------------------ |
| `OK`                 | Conversion succeeded                                                                 |
| `INVALID_ARGUMENT`   | Validation failure, unsupported `image_type`, missing deadline, or deadline < 200 ms |
| `RESOURCE_EXHAUSTED` | Queue is full                                                                        |
| `DEADLINE_EXCEEDED`  | Deadline expired while waiting in queue                                              |
| `INTERNAL`           | Image decode or WebP encode failure                                                  |
| `UNAVAILABLE`        | Server is shutting down                                                              |

## Operations

### Health Check

thumbd exposes the standard `grpc.health.v1.Health` service (always reports `SERVING`).

```sh
grpc-health-probe -addr unix:/run/thumbd/thumbd.sock
# status: SERVING
```

### Logging

Set `RUST_LOG` to control log verbosity:

```sh
RUST_LOG=thumbd=debug thumbd
```

```
DEBUG thumbd::convert: convert breakdown src_w=3024 src_h=4032 dst_w=180 dst_h=240 decode_ms=52 resize_ms=12 encode_ms=68
```

## License

[MIT License](LICENSE)
