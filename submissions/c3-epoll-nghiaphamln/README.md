# c3-epoll: C3, one epoll loop per core, its own Postgres wire protocol and gzip

- Language: [C3](https://c3-lang.org) 0.8.5_1 (official static Linux tarball, SHA-256 pinned in `install.sh`),
  `-O3 --safe=no`, single module, `--x86cpu=sse4` on the box
- Dependencies: none. libc only. The Postgres driver (`pg.c3`), the HTTP parser (`http.c3`), the JSON
  parser (`json.c3`) and the gzip encoder (`gz.c3`) are part of the program; HMAC-SHA256, SHA-256, PBKDF2 and MD5
  come from the C3 standard library.
- Front: direct (no Nginx), ports `PORT` and `EXTRA_PORT`
- Postgres connections: 64 in all (`max(2, 64 / threads)` per thread)
- Compression: gzip for bodies of 1 KB or more (in practice only `/feed`): 5.1 KB -> 2.4 KB

## Design

- **Threads**: one per CPU in the affinity mask (`sched_getaffinity`, read only), each with its own epoll set.
- **Listeners**: every thread opens its own `SO_REUSEPORT` listener on **both** ports; the kernel spreads new
  connections over the threads. A connection lives on its thread for its whole life: no locks, no hand-offs.
- **Postgres** (`pg.c3`): a client for exactly these five statements. The extended protocol over a plain TCP
  socket: Parse once per connection, then Bind/Execute with binary parameters and binary results, pipelined,
  with explicit Syncs. There is no row object: a `DataRow` is rendered to JSON straight from the socket buffer
  (`put_post_row`), so a result costs no allocation and no copy. Auth is SCRAM-SHA-256 (the box), with MD5 and
  password fallbacks.
  - Reads: a loop iteration's reads fill one connection's batch (up to 64) and share one Sync: one write, one
    round trip, one implicit transaction. The next batch goes to the least-loaded read connection.
  - Writes go to separate connections (a quarter of each thread's, at least one), each with its own Sync, so
    each is its own transaction: a like on a missing post fails alone (`23503` -> 404) and a slow commit never
    holds up reads. The response is held until that Sync's `ReadyForQuery`, which the server sends after the
    commit (rule 8).
  - A lost connection fails its queued requests with 500 and is reconnected (one try per second per thread,
    3 s connect timeout).
- **HTTP** (`http.c3`): requests are parsed in place from a per-thread 64 KB read buffer. While a request waits
  for Postgres its connection is "busy"; pipelined requests behind it wait in a per-connection buffer, so
  responses stay in order. A response is built in a per-thread arena and sent with one `write`, gzip included.
  Keep-alive connections are never timed out.
- **gzip** (`gz.c3`): greedy LZ77 with a single-probe hash table (four bytes hashed, one lookup per position,
  no chains; matches are compared eight bytes at a time) and the fixed Huffman block, in one pass. The feed
  (5.1 KB) comes out at ~2.4 KB; a per-body dynamic tree would save ~15% of the bytes but no TCP segment (two
  either way) and costs twice the instructions, so it was dropped. ~90k instructions per feed (callgrind), CRC
  slicing-by-8 included. Bodies under 1 KB are sent plain (they would not shrink).
- **Batching window**: after a loop iteration that handled fewer than 32 events, the thread sleeps 2 ms before
  the next `epoll_wait`, so one wake-up serves everything that arrived meanwhile and the reads among it share
  one Sync. A busy thread (32 events or more) never sleeps, so throughput is not capped. (5 and 10 ms were
  measured: no CPU saved at ~4.6k req/s on 4 threads, p95 doubled and quadrupled.)
- **Validation**: strict RFC 8259 JSON, HS256 JWT with signature and a required `exp` checked on every request
  (the HMAC key schedule is computed once and copied per request), body trimming as JavaScript's
  `String.trim`, ids as positive decimal integers.

## Memory per connection

- User space: one 40-byte slot per descriptor in a table sized by `RLIMIT_NOFILE` (up to 16M), allocated with
  `calloc` (pages are touched only when used, so 1M connections is 40 MB). Buffers exist only while a request
  or response is split across reads or writes.
- Per thread: a 256 KB response arena, a 256 KB gzip buffer, a 64 KB read buffer, and the Postgres receive
  buffers (256 KB each, grown on demand).

## Rules notes

- Queries: the five statements are exactly the ones in `SPEC.md` (plus `SELECT 1` for `/health`), one per
  request, prepared, pipelined. Nothing is cached across requests.
- Writes: each write commits in its own transaction before its response is sent.
- Connections: 64 to Postgres at most, well under 512.
- The box (rule 13): nothing outside the process is touched. The affinity mask is read, not set; `setrlimit`
  raises the process's own soft `RLIMIT_NOFILE` to the hard limit; socket options are `SO_REUSEPORT`,
  `SO_REUSEADDR` and `TCP_NODELAY`. No sysctl, no CPU pinning, no priorities.
- `install.sh` installs the toolchain only (c3c tarball, build-essential for the link step).

## Measured locally

Docker on an Apple-silicon Mac (Colima VM, 4 CPUs, 3 GB), Postgres 18 in a container with the box config
scaled down (`shared_buffers=384MB`); server, Postgres and the load generator share the same 4 CPUs, so the
numbers are rough.

- `test/run.sh`: `passed=42 failed=0` in a clean Ubuntu 24.04 container that ran `install.sh`.
- mload, 5,000 users over both ports, `-gzip`, 15 s ramp + 45 s hold, fresh database copy:

| think-scale | req/s | p95 | p99 | errors | server CPU per request | peak RSS |
|---|---|---|---|---|---|---|
| 0.5 | 969 | 5.0 ms | 6.4 ms | 0 | ~39 us | 3 MB |
| 0.1 | 4,653 | 4.5 ms | 5.3 ms | 0 | ~19 us | 3 MB |

- mload, 150,000 users over 4 loopback addresses x 2 ports (the VM temporarily at 8 GB for the load generator),
  `-gzip -think-scale 2.5`, 90 s ramp + 60 s hold: all 150,000 connections held, 0 errors, p95 6.9 ms; server
  RSS 10 MB (150,079 descriptors), ~12% of one CPU at ~6.5k req/s. The p99 spike (183 ms) came while the load
  generator's own 3.7 GB pushed the VM into memory pressure during the ramp.
- Syscalls per request (strace -c): 5.0 — read, write, epoll_wait, the batching nanosleep, and the Postgres
  read/write amortized over a batch. callgrind puts the server's own code at ~85k instructions per request
  outside gzip, with gzip at ~180k per feed response.
- A consistency check (pipelined requests on one connection, a body split across two writes with
  `Expect: 100-continue`, garbage and truncated requests, 300 connections dropped mid-request): every response in
  order and correct, server alive, no core dump.

## Running

```bash
sudo ./install.sh   # c3c 0.8.5_1 into /opt/c3, build-essential
./build.sh          # c3c compile *.c3 -> bin/server
DATABASE_URL=postgres://user:pass@host:5432/feed JWT_SECRET=million-users-challenge \
  HOST=0.0.0.0 PORT=80 EXTRA_PORT=81 ./start.sh
```

