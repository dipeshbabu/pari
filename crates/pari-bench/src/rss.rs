use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

/// Resident-memory samples around one benchmark phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RssSample {
    /// RSS immediately before the sampling thread starts.
    pub before_bytes: Option<u64>,
    /// Highest sampled RSS while the phase runs.
    pub peak_bytes: Option<u64>,
    /// RSS after the phase completes.
    pub after_bytes: Option<u64>,
}

/// Lightweight process-RSS sampler used by end-to-end workloads.
pub struct RssSampler {
    before_bytes: Option<u64>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<Option<u64>>>,
}

impl RssSampler {
    /// Start sampling current-process RSS roughly every five milliseconds.
    #[must_use]
    pub fn start() -> Self {
        let before_bytes = current_rss_bytes();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            let mut peak = current_rss_bytes();
            while !thread_stop.load(Ordering::Relaxed) {
                if let Some(sample) = current_rss_bytes() {
                    peak = Some(peak.map_or(sample, |current| current.max(sample)));
                }
                thread::sleep(Duration::from_millis(5));
            }
            if let Some(sample) = current_rss_bytes() {
                peak = Some(peak.map_or(sample, |current| current.max(sample)));
            }
            peak
        });

        Self {
            before_bytes,
            stop,
            handle: Some(handle),
        }
    }

    /// Stop sampling and return before, peak, and after RSS values.
    #[must_use]
    pub fn finish(mut self) -> RssSample {
        self.stop.store(true, Ordering::Relaxed);
        let peak_bytes = self
            .handle
            .take()
            .and_then(|handle| handle.join().ok())
            .flatten();
        RssSample {
            before_bytes: self.before_bytes,
            peak_bytes,
            after_bytes: current_rss_bytes(),
        }
    }
}

#[cfg(target_os = "linux")]
fn current_rss_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    parse_linux_vm_rss(&status)
}

#[cfg(not(target_os = "linux"))]
fn current_rss_bytes() -> Option<u64> {
    None
}

#[cfg(target_os = "linux")]
fn parse_linux_vm_rss(status: &str) -> Option<u64> {
    let line = status.lines().find(|line| line.starts_with("VmRSS:"))?;
    let mut fields = line.split_whitespace();
    if fields.next()? != "VmRSS:" {
        return None;
    }
    let kibibytes = fields.next()?.parse::<u64>().ok()?;
    if fields.next()? != "kB" {
        return None;
    }
    kibibytes.checked_mul(1024)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::parse_linux_vm_rss;

    #[test]
    fn parses_linux_rss_line() {
        assert_eq!(
            parse_linux_vm_rss("Name:\tpari\nVmRSS:\t  1234 kB\nThreads:\t1\n"),
            Some(1_263_616)
        );
        assert_eq!(parse_linux_vm_rss("VmRSS: nope kB"), None);
    }
}
