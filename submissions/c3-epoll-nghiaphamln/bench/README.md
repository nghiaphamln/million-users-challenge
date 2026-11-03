# Local load client

`client/` is a small Rust/Tokio HTTP/1.1 load generator for comparisons on a memory-limited Linux machine.
It follows `bench/mload`'s browsing flow: feed, think 3–7 seconds, read a post selected from that feed,
think 3–8 seconds, like with probability 15%, create with probability 2%, then idle 5–15 seconds.
All delays are multiplied by `THINK_SCALE`; the scoring workload uses 2.5. Creates use the same ISO UTC
payload shape as mload. Each user owns a keep-alive connection and reuses a token from the shared token list.

Buffers exist only during a request. Gzip, JSON, Content-Length and chunked responses are validated;
truncated or corrupt responses count as errors. A GET may retry once after a connection failure; writes
are never retried. Responses and decompressed bodies are capped at 1 MiB. This client is for the challenge's
HTTP/1.1 API, not a general HTTP client or a replacement for the official scorer.

Build and check (Rust 1.99.0 was used for the recorded runs):

```sh
cd submissions/c3-epoll-nghiaphamln/bench/client
cargo test --locked
cargo build --release --locked
```

Example inside the server's Linux network namespace, with the server listening on `0.0.0.0:3000` and `:3001`:

```sh
ulimit -n 1048576
USERS=200000 RAMP_S=80 HOLD_S=120 THINK_SCALE=2.5 THREADS=2 \
TARGETS=127.0.0.1:3000,127.0.0.1:3001,127.0.0.2:3000,127.0.0.2:3001,127.0.0.3:3000,127.0.0.3:3001,127.0.0.4:3000,127.0.0.4:3001,127.0.0.5:3000,127.0.0.5:3001,127.0.0.6:3000,127.0.0.6:3001,127.0.0.7:3000,127.0.0.7:3001,127.0.0.8:3000,127.0.0.8:3001 \
TOKENS=../../../../seed/tokens.json OUT=result.json ./target/release/feed-load
```

Multiple destination addresses avoid exhausting ephemeral source ports. The kernel must also have enough
file handles for both ends of every connection: 200,000 users on one Linux VM need over 400,000 handles.
Raising a process's `ulimit` alone does not raise the VM's system-wide limit. Docker connection tracking
can impose a separate ceiling (`nf_conntrack_max` was 65,536 on the test VM); check it when connections
plateau despite free file descriptors. These are load-test host
requirements, not settings changed by the submitted server.

The JSON includes observed peak/live connections, request counts, errors, latency histograms and ten-second
windows. A local pass additionally requires at least 99% of the requested connections to have been live at
once. Quantiles use the same 1% logarithmic buckets as mload. The client uses deterministic per-user random
streams and validates more response content than mload; use the same client for every local comparison.

Count actual connections and watch client CPU, kernel socket memory, database CPU and available VM memory.
A host that cannot allocate more sockets has reached the test environment's limit, not necessarily the
server's limit. Local ARM results do not establish an official x86 leaderboard score.

## 2026-11-04 comparison

Snapshot: the top six on the [public leaderboard](https://benchmarks.arjaythedev.com/million-users)
at 05:23 Asia/Ho_Chi_Minh on 2026-11-04. These are local ARM measurements, not new official scores.

Apple M1 Pro / 32 GiB host; Docker LinuxKit 7.0.12 ARM64 VM with 10 vCPUs, nominal 4 GiB RAM and 1 GiB swap.
App: four CPUs; PostgreSQL 18.6: four others; Rust client: two others. Each run starts a new server on a fresh
clone of the same seed database. All eight cases reached 200,000 live connections and completed with zero
request errors. Ramp 80 seconds, hold 120 seconds, think scale 2.5, gzip on both ports.

| Server | Steady app CPU/request (µs) | Whole-run p99 (ms) | Sampled app anonymous memory (MiB) |
|---|---:|---:|---:|
| C3 before | 29.14 | 7.19 | 9.6 |
| C3 + ISA-L CRC | 27.73 | 7.26 | 10.0 |
| #1 C epoll | 32.33 | 7.63 | 9.9 |
| #2 C# epoll | 38.19 | 7.05 | 44.6 |
| #3 F# io_uring | 28.57 | 17.79 | 93.5 |
| #4 Go nuclear reactor | 30.41 | 25.96 | 12.3 |
| #5 Nelua epoll | 31.95 | 7.79 | 36.3 |
| #6 Java epoll | 53.70 | 10.09 | 202.2 |

An external observer reads the app cgroup's CPU counters. CPU/request is averaged over the stable
portion of the hold, excluding ramp and connection teardown. The memory column excludes kernel socket memory and file-backed pages.
The baseline is commit `0fa592e`; the production change is the gzip CRC replacement. CPU/request fell
about 4.8% at 200K; an isolated 5,102-byte feed compression test fell from about 7.95 to 5.32 µs with
identical output. A response-local date-prefix experiment did not show a repeatable gain over CRC alone
and was not retained.

The final 300K capacity check used 120 seconds of ramp plus 300 seconds of hold and 16 destination IPs
on two ports. It observed 300,000 simultaneous connections, 4,723,024 completed requests, zero errors,
p95 10.50 ms and p99 41.44 ms. App anonymous memory peaked at a sampled 15.0 MiB; client anonymous memory
at 430.8 MiB. This run had host swap/I/O pressure, so the primary comparison remains the common 200K load.
The first 300K attempt with eight IPs stalled while opening sockets and reached only 233,419 connections;
it was correctly marked failed and is excluded. An earlier 65,536-entry conntrack ceiling was also fixed
in the lab before these measurements.

For ARM execution, C# uses the ARM64 epoll struct layout; F# uses ARM64 runtime assets and the ARM
libdeflate feature source. F# has 131,072 slots per loop (524,288 total). Java's heap is 128–512 MiB
to fit this VM. Other source logic and startup defaults are retained. C, C#, F#, Nelua and Java
come from the baseline checkout; Go nuclear reactor is pinned to `0ece5ba6f8acf558428697263e97a9ff6a0f068f`.
C# uses libdeflate 1.23-2. These adaptations and the shared VM prevent an official x86 ranking claim.

[CSV summary](results-2026-11-04.csv) · [Audit data, histograms, CPU samples and binary hashes](results-2026-11-04.json).
Each comparator has one full local run; small differences need repetition on dedicated x86 hardware.
The full local runner, build logs and snapshots are retained separately in the benchmark workspace.

Validation: all comparator builds passed 42/42 API checks; the final C3 build also passed 42/42 on
Ubuntu 24.04 x86-64 under emulation and 19 gzip round trips on each architecture. The client passed
five unit tests. x86 emulation was used only for correctness, never for the performance comparison.
Temporary kernel limits were restored and all benchmark containers were removed after the runs.
