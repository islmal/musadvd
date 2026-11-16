//! Bounded byte ring between a producer thread (image builder, file reader) and the consumer
//! (the burner), with "buffer recovery": the consumer pauses when the fill level drops below a
//! low-water mark and resumes once the producer refilled the ring to the high-water mark (or
//! ended). The fill level is what the Burn Studio shows as the main buffer.

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

/// How often a blocked side looks at the cancel flag.
const POLL: Duration = Duration::from_millis(50);

#[derive(Debug)]
struct State {
    buf: Vec<u8>,
    head: usize,
    len: usize,
    /// The producer is done (EOF for the consumer once the ring is empty).
    closed: bool,
    /// The consumer is gone: writes fail.
    abandoned: bool,
    paused: bool,
    low: usize,
    high: usize,
    /// How often the consumer had to wait for a refill.
    recoveries: u32,
}

impl State {
    fn new(capacity: usize, low_pct: u32, high_pct: u32) -> Self {
        let high_pct = high_pct.clamp(1, 100);
        let low_pct = low_pct.min(high_pct);
        State {
            buf: vec![0; capacity],
            head: 0,
            len: 0,
            closed: false,
            abandoned: false,
            // Start by filling up to the high mark (unless recovery is off).
            paused: low_pct > 0,
            low: capacity * low_pct as usize / 100,
            high: (capacity * high_pct as usize / 100).max(1),
            recoveries: 0,
        }
    }

    fn free(&self) -> usize {
        self.buf.len() - self.len
    }

    /// Copies as much of `data` as fits; returns the number of bytes taken.
    fn push(&mut self, data: &[u8]) -> usize {
        let n = data.len().min(self.free());
        let cap = self.buf.len();
        let tail = (self.head + self.len) % cap;
        let first = n.min(cap - tail);
        self.buf[tail..tail + first].copy_from_slice(&data[..first]);
        self.buf[..n - first].copy_from_slice(&data[first..n]);
        self.len += n;
        n
    }

    /// What a consumer read would do right now.
    fn pull(&mut self, out: &mut [u8]) -> Pull {
        if self.paused {
            if self.len >= self.high || self.closed {
                self.paused = false;
            } else {
                return Pull::Wait;
            }
        } else if !self.closed && self.len < self.low {
            self.paused = true;
            self.recoveries += 1;
            return Pull::Wait;
        }
        if self.len == 0 {
            return if self.closed { Pull::Eof } else { Pull::Wait };
        }
        let n = out.len().min(self.len);
        let cap = self.buf.len();
        let first = n.min(cap - self.head);
        out[..first].copy_from_slice(&self.buf[self.head..self.head + first]);
        out[first..n].copy_from_slice(&self.buf[..n - first]);
        self.head = (self.head + n) % cap;
        self.len -= n;
        Pull::Data(n)
    }

    fn percent(&self) -> u32 {
        (self.len * 100 / self.buf.len()) as u32
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Pull {
    Data(usize),
    Wait,
    Eof,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    cancel: AtomicBool,
    percent: AtomicU32,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn publish(&self, s: &State) {
        self.percent.store(s.percent(), Ordering::Relaxed);
        self.changed.notify_all();
    }
}

/// Producer end; dropping it ends the stream.
pub struct RingWriter(Arc<Shared>);
/// Consumer end; dropping it makes further writes fail.
pub struct RingReader(Arc<Shared>);

/// Cheap handle to read the fill level and to cancel both ends.
#[derive(Clone)]
pub struct RingStats(Arc<Shared>);

/// `capacity` bytes; recovery pauses below `low_pct` % until `high_pct` % (`low_pct` 0 = off).
pub fn ring(capacity: usize, low_pct: u32, high_pct: u32) -> (RingWriter, RingReader, RingStats) {
    let shared = Arc::new(Shared {
        state: Mutex::new(State::new(capacity.max(1), low_pct, high_pct)),
        changed: Condvar::new(),
        cancel: AtomicBool::new(false),
        percent: AtomicU32::new(0),
    });
    (RingWriter(shared.clone()), RingReader(shared.clone()), RingStats(shared))
}

impl RingStats {
    /// Fill level in percent.
    pub fn percent(&self) -> u32 {
        self.0.percent.load(Ordering::Relaxed)
    }

    pub fn recoveries(&self) -> u32 {
        self.0.lock().recoveries
    }

    /// Wakes and fails both ends with `ConnectionAborted` (not `Interrupted`: `write_all`/`read_exact` retry that forever).
    pub fn cancel(&self) {
        self.0.cancel.store(true, Ordering::SeqCst);
        self.0.changed.notify_all();
    }
}

fn interrupted() -> io::Error {
    io::Error::new(io::ErrorKind::ConnectionAborted, "cancelled")
}

impl Write for RingWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        let mut s = self.0.lock();
        loop {
            if self.0.cancel.load(Ordering::SeqCst) {
                return Err(interrupted());
            }
            if s.abandoned {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "the burner stopped reading"));
            }
            if s.free() > 0 {
                let n = s.push(data);
                self.0.publish(&s);
                return Ok(n);
            }
            s = self.0.changed.wait_timeout(s, POLL).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for RingWriter {
    fn drop(&mut self) {
        let mut s = self.0.lock();
        s.closed = true;
        self.0.publish(&s);
    }
}

impl Read for RingReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let mut s = self.0.lock();
        loop {
            if self.0.cancel.load(Ordering::SeqCst) {
                return Err(interrupted());
            }
            match s.pull(out) {
                Pull::Data(n) => {
                    self.0.publish(&s);
                    return Ok(n);
                }
                Pull::Eof => return Ok(0),
                Pull::Wait => s = self.0.changed.wait_timeout(s, POLL).unwrap_or_else(|e| e.into_inner()).0,
            }
        }
    }
}

impl Drop for RingReader {
    fn drop(&mut self) {
        let mut s = self.0.lock();
        s.abandoned = true;
        self.0.publish(&s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_pauses_below_low_and_resumes_at_high() {
        // 100 bytes, low 20 %, high 80 %.
        let mut s = State::new(100, 20, 80);
        let (mut out, mut pushed, mut read) = ([0u8; 10], 0, 0);
        let mut pull = |s: &mut State, read: &mut usize| {
            let r = s.pull(&mut out);
            if let Pull::Data(n) = r {
                *read += n;
            }
            r
        };
        assert_eq!(pull(&mut s, &mut read), Pull::Wait, "starts by prebuffering");
        pushed += s.push(&[1; 79]);
        assert_eq!(pull(&mut s, &mut read), Pull::Wait, "79 % < high");
        pushed += s.push(&[2; 1]);
        assert_eq!(pull(&mut s, &mut read), Pull::Data(10), "80 % reached");
        assert!(!s.paused);
        // Drain down to 20 bytes: still reading; below that it pauses.
        while s.len > 20 {
            assert!(matches!(pull(&mut s, &mut read), Pull::Data(_)));
        }
        assert_eq!(pull(&mut s, &mut read), Pull::Data(10), "20 % is not below the mark");
        assert_eq!(pull(&mut s, &mut read), Pull::Wait, "10 % < low: recover");
        assert_eq!(s.recoveries, 1);
        pushed += s.push(&[3; 50]);
        assert_eq!(pull(&mut s, &mut read), Pull::Wait, "60 % < high");
        pushed += s.push(&[3; 20]);
        assert_eq!(pull(&mut s, &mut read), Pull::Data(10));
        // At the end the rest is delivered without waiting for the high mark.
        s.closed = true;
        loop {
            match pull(&mut s, &mut read) {
                Pull::Data(_) => {}
                Pull::Eof => break,
                Pull::Wait => panic!("must not wait after close"),
            }
        }
        assert_eq!((pushed, read), (150, 150), "everything pushed is read exactly once");
    }

    #[test]
    fn wrap_around_keeps_order() {
        let mut s = State::new(8, 0, 100);
        let mut out = [0u8; 8];
        assert_eq!(s.push(&[1, 2, 3, 4, 5, 6]), 6);
        assert_eq!(s.pull(&mut out[..4]), Pull::Data(4));
        assert_eq!(&out[..4], &[1, 2, 3, 4]);
        assert_eq!(s.push(&[7, 8, 9, 10, 11, 12]), 6, "wraps around the end");
        assert_eq!(s.push(&[13]), 0, "full");
        assert_eq!(s.pull(&mut out), Pull::Data(8));
        assert_eq!(out, [5, 6, 7, 8, 9, 10, 11, 12]);
    }

    #[test]
    fn recovery_off_never_waits_for_a_refill() {
        let mut s = State::new(100, 0, 95);
        let mut out = [0u8; 4];
        s.push(&[9; 3]);
        assert_eq!(s.pull(&mut out), Pull::Data(3));
        assert_eq!(s.pull(&mut out), Pull::Wait, "empty but not closed");
        assert_eq!(s.recoveries, 0);
    }

    #[test]
    fn threads_move_data_in_order_and_report_fill() {
        let (mut w, mut r, stats) = ring(64 * 1024, 10, 90);
        let total = 5_000_000usize;
        let producer = std::thread::spawn(move || {
            let data: Vec<u8> = (0..total).map(|i| (i % 251) as u8).collect();
            for chunk in data.chunks(7919) {
                w.write_all(chunk).unwrap();
            }
        });
        let mut got = Vec::with_capacity(total);
        let mut buf = vec![0u8; 10_000];
        let mut max_fill = 0;
        loop {
            let n = r.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            got.extend_from_slice(&buf[..n]);
            max_fill = max_fill.max(stats.percent());
        }
        producer.join().unwrap();
        assert_eq!(got.len(), total);
        assert!(got.iter().enumerate().all(|(i, &b)| b == (i % 251) as u8));
        assert!(max_fill > 0 && max_fill <= 100);
    }

    #[test]
    fn cancel_and_dropped_reader_unblock_the_producer() {
        let (mut w, r, stats) = ring(16, 0, 100);
        w.write_all(&[0; 16]).unwrap();
        let t = std::thread::spawn(move || w.write_all(&[1; 16]).unwrap_err().kind());
        std::thread::sleep(Duration::from_millis(100));
        drop(r);
        assert_eq!(t.join().unwrap(), io::ErrorKind::BrokenPipe);

        let (mut w, mut r, stats2) = ring(16, 0, 100);
        let t = std::thread::spawn(move || r.read(&mut [0u8; 4]).unwrap_err().kind());
        std::thread::sleep(Duration::from_millis(100));
        stats2.cancel();
        assert_eq!(t.join().unwrap(), io::ErrorKind::ConnectionAborted);
        assert_eq!(w.write(&[1]).unwrap_err().kind(), io::ErrorKind::ConnectionAborted);
        // write_all must not retry an abort (it did, spinning forever, with Interrupted).
        assert_eq!(w.write_all(&[1]).unwrap_err().kind(), io::ErrorKind::ConnectionAborted);
        assert_eq!(stats.percent(), 100);
    }
}
