use governor::{
    Quota, RateLimiter,
    clock::{Clock, DefaultClock},
    state::InMemoryState,
    state::NotKeyed,
};
use pin_project::pin_project;
use std::collections::HashMap;
use std::num::NonZeroU32;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::Mutex;
use uuid::Uuid;

type SharedLimiter = Arc<RateLimiter<NotKeyed, InMemoryState, DefaultClock>>;

const CHUNK: u32 = 16384;
const REQUEST_JITTER_PERCENT: u32 = 10;
static JITTER_STATE: AtomicU64 = AtomicU64::new(0x9E37_79B9_7F4A_7C15);

struct DirectionalLimiters {
    read: Option<SharedLimiter>,
    write: Option<SharedLimiter>,
}

/// Maintains one shared governor rate limiter per tunnel-id so all
/// connections for the same tunnel share bandwidth quotas.
/// Read/upload and write/download are limited independently.
#[derive(Clone, Default)]
pub struct RateLimiterPool {
    limiters: Arc<Mutex<HashMap<Uuid, DirectionalLimiters>>>,
}

impl RateLimiterPool {
    pub fn new() -> Self {
        Self::default()
    }

    /// Wrap reader/writer with a shared rate limiter for the given tunnel-id.
    /// `m` is the rate in MiB/s.
    pub async fn wrap<R, W>(
        &self,
        tunnel_id: Uuid,
        upload_m: Option<u16>,
        download_m: Option<u16>,
        reader: R,
        writer: W,
    ) -> (RateLimitedRead<R>, RateLimitedWrite<W>) {
        fn build_limiter(m: Option<u16>) -> Option<SharedLimiter> {
            m.map(|v| {
                let bytes_per_sec = u64::from(v) * 1024 * 1024;
                let bps = (bytes_per_sec as u32).max(1);
                let burst = ((bytes_per_sec as f64 * 1.2) as u32).max(1);
                let quota =
                    Quota::per_second(NonZeroU32::new(bps).unwrap()).allow_burst(NonZeroU32::new(burst).unwrap());
                Arc::new(RateLimiter::direct(quota))
            })
        }

        let (read_limiter, write_limiter) = {
            let mut map = self.limiters.lock().await;
            let limiters = map
                .entry(tunnel_id)
                .or_insert_with(|| {
                    DirectionalLimiters {
                        // reader path carries remote->client traffic => download
                        read: build_limiter(download_m),
                        // writer path carries client->remote traffic => upload
                        write: build_limiter(upload_m),
                    }
                });
            (limiters.read.clone(), limiters.write.clone())
        };
        (
            RateLimitedRead {
                inner: reader,
                limiter: read_limiter,
                sleep: None,
            },
            RateLimitedWrite {
                inner: writer,
                limiter: write_limiter,
                sleep: None,
            },
        )
    }
}

#[pin_project]
pub struct RateLimitedRead<R> {
    #[pin]
    inner: R,
    limiter: Option<SharedLimiter>,
    sleep: Option<Pin<Box<tokio::time::Sleep>>>,
}

#[pin_project]
pub struct RateLimitedWrite<W> {
    #[pin]
    inner: W,
    limiter: Option<SharedLimiter>,
    sleep: Option<Pin<Box<tokio::time::Sleep>>>,
}

fn schedule_wait(sleep: &mut Option<Pin<Box<tokio::time::Sleep>>>, cx: &mut Context<'_>, wait: std::time::Duration) {
    *sleep = Some(Box::pin(tokio::time::sleep(wait)));
    cx.waker().wake_by_ref();
}

fn next_jitter_roll() -> u32 {
    let mut cur = JITTER_STATE.load(Ordering::Relaxed);
    loop {
        // xorshift64*
        let mut x = cur;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        match JITTER_STATE.compare_exchange_weak(cur, x, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return x as u32,
            Err(actual) => cur = actual,
        }
    }
}

fn jittered_chunk(base: u32) -> u32 {
    let base = base.clamp(1, CHUNK);
    let span = REQUEST_JITTER_PERCENT * 2 + 1; // e.g. 21 for +/-10%
    let pct = 100 - REQUEST_JITTER_PERCENT + (next_jitter_roll() % span);
    let n = ((base as u64 * pct as u64) / 100) as u32;
    n.clamp(1, CHUNK)
}

impl<R: AsyncRead> AsyncRead for RateLimitedRead<R> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        let this = self.project();

        if let Some(sleep) = this.sleep.as_mut() {
            match sleep.as_mut().poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(()) => *this.sleep = None,
            }
        }

        let n = jittered_chunk((buf.remaining() as u32).min(CHUNK).max(1)).min(buf.remaining() as u32);
        if let Some(limiter) = this.limiter.as_ref() {
            let clock = DefaultClock::default();
            match limiter.check_n(NonZeroU32::new(n).unwrap()) {
                Err(_) => {
                    schedule_wait(this.sleep, cx, std::time::Duration::from_millis(10));
                    return Poll::Pending;
                }
                Ok(Err(not_until)) => {
                    schedule_wait(this.sleep, cx, not_until.wait_time_from(clock.now()));
                    return Poll::Pending;
                }
                Ok(Ok(())) => {}
            }
        }

        let before = buf.filled().len();
        let mut limited_buf = buf.take(n as usize);
        let result = this.inner.poll_read(cx, &mut limited_buf);
        let read_n = limited_buf.filled().len();
        let new_filled = before + read_n;
        unsafe { buf.assume_init(new_filled) };
        buf.set_filled(new_filled);
        result
    }
}

impl<W: AsyncWrite> AsyncWrite for RateLimitedWrite<W> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        let this = self.project();

        if let Some(sleep) = this.sleep.as_mut() {
            match sleep.as_mut().poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(()) => *this.sleep = None,
            }
        }

        let n = jittered_chunk((buf.len() as u32).min(CHUNK).max(1)).min(buf.len() as u32);
        if let Some(limiter) = this.limiter.as_ref() {
            let clock = DefaultClock::default();
            match limiter.check_n(NonZeroU32::new(n).unwrap()) {
                Err(_) => {
                    schedule_wait(this.sleep, cx, std::time::Duration::from_millis(10));
                    return Poll::Pending;
                }
                Ok(Err(not_until)) => {
                    schedule_wait(this.sleep, cx, not_until.wait_time_from(clock.now()));
                    return Poll::Pending;
                }
                Ok(Ok(())) => {}
            }
        }

        this.inner.poll_write(cx, &buf[..n as usize])
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.project().inner.poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.project().inner.poll_shutdown(cx)
    }
}
