//! The daemon's bounded line log.
use super::*;

pub(crate) const LOG_QUEUE_CAPACITY: usize = 64;
pub(crate) const LOG_LINE_LIMIT: usize = 4096;
pub(crate) enum LogMessage {
    Line(String),
    Flush(mpsc::Sender<()>),
}
pub(crate) static LOGGER: std::sync::OnceLock<LineLogger> = std::sync::OnceLock::new();
pub(crate) struct LineLogger {
    pub(crate) sender: mpsc::SyncSender<LogMessage>,
    pub(crate) dropped: std::sync::atomic::AtomicU64,
}
impl LineLogger {
    pub(crate) fn new(mut sink: impl Write + Send + 'static, capacity: usize) -> Self {
        let (sender, receiver) = mpsc::sync_channel::<LogMessage>(capacity);
        thread::spawn(move || {
            for message in receiver {
                match message {
                    LogMessage::Line(line) => {
                        let _ = writeln!(sink, "{line}");
                    }
                    LogMessage::Flush(done) => {
                        let _ = sink.flush();
                        let _ = done.send(());
                    }
                }
            }
        });
        Self {
            sender,
            dropped: std::sync::atomic::AtomicU64::new(0),
        }
    }
    pub(crate) fn flush(&self, deadline: Duration) -> bool {
        let started = std::time::Instant::now();
        let (done, received) = mpsc::channel();
        let mut barrier = LogMessage::Flush(done);
        loop {
            let remaining = deadline.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return false;
            }
            match self.sender.try_send(barrier) {
                Ok(()) => break,
                Err(mpsc::TrySendError::Full(message)) => {
                    barrier = message;
                    thread::sleep(remaining.min(Duration::from_millis(1)));
                }
                Err(mpsc::TrySendError::Disconnected(_)) => return false,
            }
        }
        received
            .recv_timeout(deadline.saturating_sub(started.elapsed()))
            .is_ok()
    }
    pub(crate) fn send(&self, line: &str) {
        if line.len() > LOG_LINE_LIMIT
            || self
                .sender
                .try_send(LogMessage::Line(line.to_owned()))
                .is_err()
        {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}
/// A single writer owns stderr writes. Requests never wait for that sink or queue space; memory is bounded.
/// On SIGTERM, queued lines not yet written may be lost; at most the 64-line queue.
/// Silent in test builds, whose output stderr would bypass capture.
pub(crate) fn log_line(line: &str) {
    if !cfg!(test) {
        LOGGER
            .get_or_init(|| LineLogger::new(std::io::stderr(), LOG_QUEUE_CAPACITY))
            .send(line);
    }
}
/// Drain previously accepted lines, with one deadline covering queue space and sink acknowledgement.
/// A blocked sink can outlive the deadline; the caller never joins the writer thread.
pub fn flush_log(deadline: Duration) {
    if let Some(logger) = LOGGER.get() {
        logger.flush(deadline);
    }
}
pub(crate) struct LogDrain;
impl Drop for LogDrain {
    fn drop(&mut self) {
        flush_log(Duration::from_millis(250));
    }
}
