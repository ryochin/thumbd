# thumbd-client

A CLI client for the [thumbd](../README.md) image conversion service.

## Building

```sh
cargo build --release -p thumbd-client
```

Artifact: `target/release/thumbd-client`

## Usage

Convert an image to WebP and write the result alongside the source file:

```sh
# Basic usage (writes photo.webp)
thumbd-client photo.jpg

# Connect via TCP
thumbd-client photo.jpg --server http://localhost:50051

# Specify output size and quality
thumbd-client photo.jpg --max-width 800 --max-height 600 --quality 85

# Overwrite without prompting
thumbd-client photo.jpg --force
```

## Options

| Flag                 | Default                                                                        | Description                                                                        |
| -------------------- | ------------------------------------------------------------------------------ | ---------------------------------------------------------------------------------- |
| `-s`, `--server`     | Linux: `unix:/run/thumbd/thumbd.sock`<br>macOS: `unix:/tmp/thumbd/thumbd.sock` | Server address                                                 |
| `-W`, `--max-width`  | `320`                                                                          | Maximum output width (px)                                      |
| `-H`, `--max-height` | `240`                                                                          | Maximum output height (px)                                     |
| `-q`, `--quality`    | — (server default: 80)                                                         | WebP quality (1–100)                                           |
| `-e`, `--effort`     | —                                                                              | WebP encoding effort (1–6; higher = better compression, slower; server default: 3) |
| `-d`, `--deadline`   | `5000`                                                                         | Request deadline (ms)                                          |
| `--force`            | —                                                                              | Overwrite output file without prompting                        |
| `--bench`            | —                                                                              | Enable benchmark mode; sets concurrency                        |
| `--requests`         | same as `--bench`                                                              | Total number of requests to send (benchmark only)              |

## Benchmark Mode

Send multiple concurrent requests to measure throughput and latency:

```sh
# 100 requests at concurrency 8
thumbd-client photo.jpg --bench 8 --requests 100

# 4 requests at concurrency 4 (--requests defaults to --bench value)
thumbd-client photo.jpg --bench 4
```

Example output:

```
benchmarking: 100 requests, concurrency=8
  ok:       100  (100.0%)
  err:         0  (0.0%)
  elapsed: 3241ms
  rps:     30.9
  latency (client-side, ok only):
    min= 148ms  p50= 201ms  p95= 312ms  p99= 389ms  max= 412ms
  work_ms (server-side, ok only):
    min=  89ms  p50= 120ms  p95= 198ms  p99= 230ms  max= 251ms
```
