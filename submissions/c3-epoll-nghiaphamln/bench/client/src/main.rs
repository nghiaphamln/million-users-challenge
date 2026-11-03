//! Local comparison load, following bench/mload's browsing flow. Not an official score generator.
use serde_json::{Value, json};
use std::{
    env, fs,
    io::{self, Read},
    net::{SocketAddr, ToSocketAddrs},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering::Relaxed},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::{sleep, sleep_until, timeout},
};

const BINS: usize = 2048;
const MAX_BODY: usize = 1024 * 1024;
const NAMES: [&str; 4] = [
    "GET /feed",
    "GET /posts/:id",
    "POST /posts/:id/like",
    "POST /posts",
];
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
fn bad(s: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, s)
}
fn number<T: std::str::FromStr>(key: &str, default: &str) -> T {
    env::var(key)
        .unwrap_or_else(|_| default.into())
        .parse()
        .unwrap_or_else(|_| panic!("invalid {key}"))
}
// UTC ISO timestamp, matching mload's create-post payload without a date-library dependency.
fn timestamp_ms(ms: u128) -> String {
    let days = (ms / 86400000) as u64 + 719468;
    let era = days / 146097;
    let doe = days - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    let seconds = (ms / 1000) % 86400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60,
        ms % 1000
    )
}
struct Token {
    name: String,
    auth: String,
}
struct Config {
    targets: Vec<SocketAddr>,
    tokens: Vec<Token>,
    users: usize,
    ramp: f64,
    hold: f64,
    think: f64,
    deadline: Instant,
}
struct Stats {
    hist: [AtomicU64; BINS],
    ok: [AtomicU64; 4],
    fail: [AtomicU64; 4],
    live: AtomicU64,
    peak: AtomicU64,
    dials: AtomicU64,
    transport: AtomicU64,
    timeouts: AtomicU64,
}
impl Stats {
    fn new() -> Self {
        Self {
            hist: std::array::from_fn(|_| AtomicU64::new(0)),
            ok: std::array::from_fn(|_| AtomicU64::new(0)),
            fail: std::array::from_fn(|_| AtomicU64::new(0)),
            live: AtomicU64::new(0),
            peak: AtomicU64::new(0),
            dials: AtomicU64::new(0),
            transport: AtomicU64::new(0),
            timeouts: AtomicU64::new(0),
        }
    }
    fn record(&self, ep: usize, success: bool, elapsed: Duration) {
        let i = ((elapsed.as_secs_f64() * 1e6).max(1.0).ln() / 1.01f64.ln()).floor() as usize;
        self.hist[i.min(BINS - 1)].fetch_add(1, Relaxed);
        (if success {
            &self.ok[ep]
        } else {
            &self.fail[ep]
        })
        .fetch_add(1, Relaxed);
    }
    fn snapshot(&self) -> Vec<u64> {
        self.hist.iter().map(|x| x.load(Relaxed)).collect()
    }
}
fn percentile(hist: &[u64], p: f64) -> f64 {
    let total: u64 = hist.iter().sum();
    if total == 0 {
        return 0.0;
    }
    let want = (total as f64 * p).ceil() as u64;
    let mut n = 0;
    for (i, count) in hist.iter().enumerate() {
        n += count;
        if n >= want {
            return 1.01f64.powf(i as f64 + 0.5) / 1000.0;
        }
    }
    0.0
}
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }
    fn fraction(&mut self) -> f64 {
        (self.next() >> 11) as f64 / ((1u64 << 53) as f64)
    }
    fn delay(&mut self, lo: f64, hi: f64, scale: f64) -> Duration {
        Duration::from_secs_f64((lo + self.fraction() * (hi - lo)) * scale)
    }
}
struct Connection {
    stream: TcpStream,
    stats: Arc<Stats>,
}
impl Drop for Connection {
    fn drop(&mut self) {
        self.stats.live.fetch_sub(1, Relaxed);
    }
}
struct Frame {
    status: u16,
    body: Vec<u8>,
    gzip: bool,
    close: bool,
}

// Complete one Content-Length/chunked response. No persistent per-connection receive buffers.
fn frame(raw: &[u8]) -> Result<Option<Frame>> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut response = httparse::Response::new(&mut headers);
    let start = match response.parse(raw)? {
        httparse::Status::Partial => {
            if raw.len() > 64 * 1024 {
                return Err(bad("headers too large").into());
            }
            return Ok(None);
        }
        httparse::Status::Complete(n) => n,
    };
    let mut length = None;
    let (mut chunked, mut gzip, mut close) = (false, false, false);
    for h in response.headers.iter() {
        let v = std::str::from_utf8(h.value)?.trim();
        if h.name.eq_ignore_ascii_case("content-length") {
            length = Some(v.parse::<usize>()?);
        }
        if h.name.eq_ignore_ascii_case("transfer-encoding") {
            chunked = v.eq_ignore_ascii_case("chunked");
        }
        if h.name.eq_ignore_ascii_case("content-encoding") {
            gzip = v.eq_ignore_ascii_case("gzip");
        }
        if h.name.eq_ignore_ascii_case("connection") {
            close = v.eq_ignore_ascii_case("close");
        }
    }
    let status = response.code.ok_or_else(|| bad("missing status"))?;
    let body = if chunked {
        let mut at = start;
        let mut result = Vec::new();
        loop {
            let Some(end) = raw[at..].windows(2).position(|w| w == b"\r\n") else {
                return Ok(None);
            };
            let line = std::str::from_utf8(&raw[at..at + end])?;
            let size = usize::from_str_radix(line.split(';').next().unwrap().trim(), 16)?;
            at += end + 2;
            if size == 0 {
                loop {
                    let Some(end) = raw[at..].windows(2).position(|w| w == b"\r\n") else {
                        return Ok(None);
                    };
                    at += end + 2;
                    if end == 0 {
                        break;
                    }
                }
                break;
            }
            if size > MAX_BODY - result.len() {
                return Err(bad("chunked body too large").into());
            }
            if raw.len() - at < size + 2 {
                return Ok(None);
            }
            if &raw[at + size..at + size + 2] != b"\r\n" {
                return Err(bad("bad chunk ending").into());
            }
            result.extend_from_slice(&raw[at..at + size]);
            at += size + 2;
        }
        result
    } else {
        let len = length.ok_or_else(|| bad("response needs Content-Length or chunked encoding"))?;
        if len > MAX_BODY {
            return Err(bad("body too large").into());
        }
        if raw.len() - start < len {
            return Ok(None);
        }
        raw[start..start + len].to_vec()
    };
    Ok(Some(Frame {
        status,
        body,
        gzip,
        close,
    }))
}
fn decode(f: Frame, feed: bool, rng: &mut Rng) -> Result<(u16, u64, bool)> {
    let body = if f.gzip {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&f.body[..])
            .take(MAX_BODY as u64 + 1)
            .read_to_end(&mut out)?;
        if out.len() > MAX_BODY {
            return Err(bad("inflated body too large").into());
        }
        out
    } else {
        f.body
    };
    let value: Value = serde_json::from_slice(&body)?;
    let id = if feed && f.status == 200 {
        let posts = value["posts"]
            .as_array()
            .filter(|p| p.len() == 20)
            .ok_or_else(|| bad("feed must contain 20 posts"))?;
        posts[rng.next() as usize % posts.len()]["id"]
            .as_u64()
            .filter(|id| *id > 0)
            .ok_or_else(|| bad("bad post id"))?
    } else {
        0
    };
    Ok((f.status, id, f.close))
}
async fn exchange(
    stream: &mut TcpStream,
    request: &[u8],
    feed: bool,
    rng: &mut Rng,
) -> Result<(u16, u64, bool)> {
    stream.write_all(request).await?;
    let mut raw = Vec::with_capacity(4096);
    loop {
        // Keep the buffer on the heap: an inline 8 KB array would make every idle task 8 KB too.
        if raw.len() > MAX_BODY + 64 * 1024 {
            return Err(bad("response too large").into());
        }
        raw.reserve(4096);
        if stream.read_buf(&mut raw).await? == 0 {
            return Err(bad("unexpected EOF").into());
        }
        if let Some(f) = frame(&raw)? {
            return decode(f, feed, rng);
        }
    }
}
async fn request(
    c: &mut Option<Connection>,
    cfg: &Config,
    stats: &Arc<Stats>,
    user: usize,
    ep: usize,
    id: u64,
    iter: u64,
    rng: &mut Rng,
) -> Option<u64> {
    let token = &cfg.tokens[user % cfg.tokens.len()];
    let method = if ep < 2 { "GET" } else { "POST" };
    let path = match ep {
        0 => "/feed".to_string(),
        1 => format!("/posts/{id}"),
        2 => format!("/posts/{id}/like"),
        _ => "/posts".to_string(),
    };
    let payload = if ep == 3 {
        json!({"body":format!("{} says hi at {} (VU {}, iter {})",token.name,timestamp_ms(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis()),user+1,iter)}).to_string()
    } else {
        String::new()
    };
    let mut wire = format!(
        "{method} {path} HTTP/1.1\r\nHost: {}\r\nUser-Agent: mload\r\nAccept-Encoding: gzip\r\n",
        cfg.targets[user % cfg.targets.len()].ip()
    );
    if ep >= 2 {
        wire.push_str(&token.auth);
    }
    if ep == 3 {
        wire.push_str("Content-Type: application/json\r\n");
    }
    if ep >= 2 {
        wire.push_str(&format!("Content-Length: {}\r\n", payload.len()));
    }
    wire.push_str("\r\n");
    wire.push_str(&payload);
    let mut result = None;
    let mut elapsed = Duration::ZERO;
    let mut timed_out = false;
    for attempt in 0..2 {
        if c.is_none() {
            stats.dials.fetch_add(1, Relaxed);
            match timeout(
                Duration::from_secs(60),
                TcpStream::connect(cfg.targets[user % cfg.targets.len()]),
            )
            .await
            {
                Ok(Ok(stream)) => {
                    if stream.set_nodelay(true).is_err() {
                        break;
                    }
                    let live = stats.live.fetch_add(1, Relaxed) + 1;
                    stats.peak.fetch_max(live, Relaxed);
                    *c = Some(Connection {
                        stream,
                        stats: stats.clone(),
                    });
                }
                Err(_) => {
                    timed_out = true;
                    break;
                }
                _ => break,
            }
        }
        let start = Instant::now();
        let answer = timeout(
            Duration::from_secs(60),
            exchange(
                &mut c.as_mut().unwrap().stream,
                wire.as_bytes(),
                ep == 0,
                rng,
            ),
        )
        .await;
        elapsed += start.elapsed();
        match answer {
            Ok(Ok((status, post, close))) => {
                if close {
                    *c = None;
                }
                result = Some((status, post));
                break;
            }
            Err(_) => {
                timed_out = true;
                *c = None;
                break;
            }
            Ok(Err(_)) => {
                *c = None;
                if ep >= 2 || attempt == 1 {
                    break;
                }
            }
        }
    }
    if Instant::now() >= cfg.deadline {
        return None;
    }
    let success = result.is_some_and(|(s, _)| s < 400 && s >= 200);
    stats.record(ep, success, elapsed);
    if result.is_none() {
        stats.transport.fetch_add(1, Relaxed);
    }
    if timed_out {
        stats.timeouts.fetch_add(1, Relaxed);
    }
    if success {
        result.map(|(_, id)| id)
    } else {
        None
    }
}
async fn user(n: usize, cfg: Arc<Config>, stats: Arc<Stats>) {
    let mut rng = Rng(82 ^ n as u64);
    let mut c = None;
    let mut iter = 0;
    sleep(rng.delay(0.0, 5.0, cfg.think)).await;
    while Instant::now() < cfg.deadline {
        let Some(id) = request(&mut c, &cfg, &stats, n, 0, 0, iter, &mut rng).await else {
            sleep(rng.delay(3.0, 7.0, cfg.think)).await;
            continue;
        };
        sleep(rng.delay(3.0, 7.0, cfg.think)).await;
        if Instant::now() >= cfg.deadline {
            break;
        }
        request(&mut c, &cfg, &stats, n, 1, id, iter, &mut rng).await;
        sleep(rng.delay(3.0, 8.0, cfg.think)).await;
        if Instant::now() >= cfg.deadline {
            break;
        }
        if rng.fraction() < 0.15 {
            request(&mut c, &cfg, &stats, n, 2, id, iter, &mut rng).await;
        }
        if rng.fraction() < 0.02 {
            request(&mut c, &cfg, &stats, n, 3, id, iter, &mut rng).await;
        }
        iter += 1;
        sleep(rng.delay(5.0, 15.0, cfg.think)).await;
    }
}
async fn run(cfg: Arc<Config>, output: String) -> Result<()> {
    let stats = Arc::new(Stats::new());
    eprintln!(
        "user_future_bytes={} users={} ramp={} hold={} think={}",
        std::mem::size_of_val(&user(0, cfg.clone(), stats.clone())),
        cfg.users,
        cfg.ramp,
        cfg.hold,
        cfg.think
    );
    let start = Instant::now();
    let report = stats.clone();
    let timeline = Arc::new(Mutex::new(Vec::<Value>::new()));
    let ticks = timeline.clone();
    tokio::spawn(async move {
        let mut previous = 0;
        let mut previous_at = start;
        let mut previous_hist = vec![0; BINS];
        loop {
            sleep(Duration::from_secs(10)).await;
            let hist = report.snapshot();
            let total: u64 = hist.iter().sum();
            let now = Instant::now();
            eprintln!(
                "t={} live={} req/s={:.0} p95={:.2}ms p99={:.2}ms",
                start.elapsed().as_secs(),
                report.live.load(Relaxed),
                (total - previous) as f64 / now.duration_since(previous_at).as_secs_f64(),
                percentile(&hist, 0.95),
                percentile(&hist, 0.99)
            );
            let window: Vec<u64> = hist
                .iter()
                .zip(&previous_hist)
                .map(|(a, b)| a - b)
                .collect();
            ticks.lock().unwrap().push(json!({"t":start.elapsed().as_secs_f64(),"reqs":total,"conns":report.live.load(Relaxed),"window_reqs":total-previous,"p95_ms":percentile(&window,0.95),"p99_ms":percentile(&window,0.99)}));
            previous_hist = hist;
            previous_at = now;
            previous = total;
        }
    });
    // Ramp in small groups, with the exact deadline derived from user index (no accumulated sleep drift).
    for n in 0..cfg.users {
        if n % 100 == 0 {
            sleep_until(
                (start + Duration::from_secs_f64(cfg.ramp * n as f64 / cfg.users as f64)).into(),
            )
            .await;
        }
        tokio::spawn(user(n, cfg.clone(), stats.clone()));
    }
    sleep_until(cfg.deadline.into()).await;
    let hist = stats.snapshot();
    let total: u64 = hist.iter().sum();
    let fails: u64 = stats.fail.iter().map(|x| x.load(Relaxed)).sum();
    let err = if total == 0 {
        100.0
    } else {
        fails as f64 * 100.0 / total as f64
    };
    let p95 = percentile(&hist, 0.95);
    let p99 = percentile(&hist, 0.99);
    let peak = stats.peak.load(Relaxed);
    let endpoints:Vec<_> = (0..4).map(|i|json!({"name":NAMES[i],"ok":stats.ok[i].load(Relaxed),"fail":stats.fail[i].load(Relaxed)})).collect();
    let result = json!({"users":cfg.users,"peak_connections":peak,"connections_at_end":stats.live.load(Relaxed),"duration_s":start.elapsed().as_secs_f64(),"reqs":total,"fails":fails,"req_per_s":total as f64/start.elapsed().as_secs_f64(),"p95_ms":p95,"p99_ms":p99,"err_pct":err,"pass":p95<500.0 && p99<1000.0 && err<1.0 && peak*100>=cfg.users as u64*99,"dials":stats.dials.load(Relaxed),"transport_errors":stats.transport.load(Relaxed),"timeouts":stats.timeouts.load(Relaxed),"endpoints":endpoints,"hist":hist,"timeline":timeline.lock().unwrap().clone()});
    fs::write(output, serde_json::to_vec_pretty(&result)?)?;
    println!(
        "{}",
        json!({"reqs":total,"p95_ms":p95,"p99_ms":p99,"err_pct":err,"peak_connections":peak,"pass":result["pass"]})
    );
    Ok(())
}
fn main() -> Result<()> {
    let users = number("USERS", "5000");
    let ramp: f64 = number("RAMP_S", "30");
    let hold: f64 = number("HOLD_S", "60");
    let think: f64 = number("THINK_SCALE", "2.5");
    if users == 0
        || !ramp.is_finite()
        || ramp < 0.0
        || !hold.is_finite()
        || hold <= 0.0
        || !think.is_finite()
        || think <= 0.0
    {
        return Err(bad("invalid load configuration").into());
    }
    let mut targets = Vec::new();
    for target in env::var("TARGETS")
        .unwrap_or_else(|_| "127.0.0.1:3000,127.0.0.1:3001".into())
        .split(',')
    {
        targets.push(
            target
                .to_socket_addrs()?
                .next()
                .ok_or_else(|| bad("target has no address"))?,
        );
    }
    let values: Value = serde_json::from_slice(&fs::read(env::var("TOKENS")?)?)?;
    let mut tokens = Vec::new();
    for v in values
        .as_array()
        .ok_or_else(|| bad("tokens must be an array"))?
    {
        let name = v["username"]
            .as_str()
            .ok_or_else(|| bad("missing username"))?
            .to_string();
        let token = v["token"].as_str().ok_or_else(|| bad("missing token"))?;
        if token.contains(['\r', '\n']) {
            return Err(bad("invalid token header").into());
        }
        tokens.push(Token {
            name,
            auth: format!("Authorization: Bearer {token}\r\n"),
        });
    }
    if tokens.is_empty() {
        return Err(bad("no tokens").into());
    }
    let cfg = Arc::new(Config {
        targets,
        tokens,
        users,
        ramp,
        hold,
        think,
        deadline: Instant::now() + Duration::from_secs_f64(ramp + hold),
    });
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(number("THREADS", "2"))
        .enable_all()
        .build()?
        .block_on(run(
            cfg,
            env::var("OUT").unwrap_or_else(|_| "result.json".into()),
        ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn utc_payload_timestamps() {
        assert_eq!(timestamp_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(timestamp_ms(951782400123), "2000-02-29T00:00:00.123Z");
        assert_eq!(timestamp_ms(4107542400123), "2100-03-01T00:00:00.123Z");
    }
    #[test]
    fn fragmented_content_length() {
        let wire = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}";
        for n in 0..wire.len() {
            assert!(frame(&wire[..n]).unwrap().is_none());
        }
        assert_eq!(frame(wire).unwrap().unwrap().body, b"{}");
    }
    #[test]
    fn fragmented_chunked_with_trailer() {
        let wire=b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1;ext=x\r\n{\r\n1\r\n}\r\n0\r\nX-Test: yes\r\n\r\n";
        for n in 0..wire.len() {
            assert!(frame(&wire[..n]).unwrap().is_none());
        }
        assert_eq!(frame(wire).unwrap().unwrap().body, b"{}");
    }
    #[test]
    fn reject_oversized_and_malformed() {
        assert!(frame(b"HTTP/1.1 200 OK\r\nContent-Length: 999999999\r\n\r\n").is_err());
        assert!(frame(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nxXX").is_err());
        let f = Frame {
            status: 200,
            body: vec![0; 30],
            gzip: true,
            close: false,
        };
        assert!(decode(f, true, &mut Rng(82)).is_err());
    }
    #[test]
    fn feed_validation_and_histogram() {
        let posts: Vec<_> = (1..=20).map(|id| json!({"id":id})).collect();
        let f = Frame {
            status: 200,
            body: json!({"posts":posts}).to_string().into_bytes(),
            gzip: false,
            close: false,
        };
        assert!((1..=20).contains(&decode(f, true, &mut Rng(82)).unwrap().1));
        let s = Stats::new();
        for _ in 0..100 {
            s.record(0, true, Duration::from_millis(5));
        }
        assert!((4.95..5.05).contains(&percentile(&s.snapshot(), 0.99)));
    }
}
