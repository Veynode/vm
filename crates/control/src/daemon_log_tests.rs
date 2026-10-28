use super::*;
#[test]
fn bounded_logger_returns_and_counts_drops_when_sink_blocks_forever() {
    struct Blocked(mpsc::Sender<()>);
    impl Write for Blocked {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            self.0.send(()).unwrap();
            loop {
                thread::park();
            }
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let (entered, waiting) = mpsc::channel();
    let logger = Arc::new(LineLogger::new(Blocked(entered), 2));
    logger.send("first");
    waiting.recv_timeout(Duration::from_secs(1)).unwrap();
    let producer = logger.clone();
    let (done, returned) = mpsc::channel();
    let caller = thread::spawn(move || {
        for _ in 0..10 {
            producer.send("queued");
        }
        producer.send(&"x".repeat(LOG_LINE_LIMIT + 1));
        done.send(()).unwrap();
    });
    returned.recv_timeout(Duration::from_secs(1)).unwrap();
    caller.join().unwrap();
    assert_eq!(logger.dropped.load(Ordering::Relaxed), 9);
}
#[test]
fn logger_flush_writes_queued_lines_and_flushes_sink_before_acknowledging() {
    #[derive(Clone)]
    struct Recording(Arc<Mutex<(Vec<u8>, bool)>>);
    impl Write for Recording {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.0.lock().unwrap().1 = true;
            Ok(())
        }
    }
    let sink = Recording(Arc::new(Mutex::new((Vec::new(), false))));
    let logger = LineLogger::new(sink.clone(), 4);
    logger.send("first");
    logger.send("second");
    assert!(logger.flush(Duration::from_secs(1)));
    let recorded = sink.0.lock().unwrap();
    assert_eq!(recorded.0, b"first\nsecond\n");
    assert!(recorded.1);
    assert_eq!(logger.dropped.load(Ordering::Relaxed), 0);
}
#[test]
fn logger_flush_deadline_bounds_full_queue_and_blocked_sink_acknowledgement() {
    struct Blocked(mpsc::Sender<()>);
    impl Write for Blocked {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            self.0.send(()).unwrap();
            loop {
                thread::park();
            }
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    for capacity in [1, 2] {
        let (entered, waiting) = mpsc::channel();
        let logger = LineLogger::new(Blocked(entered), capacity);
        logger.send("blocked");
        waiting.recv_timeout(Duration::from_secs(1)).unwrap();
        logger.send("queued");
        let (done, returned) = mpsc::channel();
        let caller = thread::spawn(move || {
            let started = std::time::Instant::now();
            assert!(!logger.flush(Duration::from_millis(50)));
            done.send(started.elapsed()).unwrap();
        });
        let elapsed = returned.recv_timeout(Duration::from_millis(250)).unwrap();
        // Allow scheduling overhead, but never wait for the permanently blocked writer.
        assert!(elapsed < Duration::from_millis(250), "{elapsed:?}");
        caller.join().unwrap();
    }
}
