use super::*;
use serde_json::json;

/// An encoder the test drives: it records commands and emits what the test
/// pushes.
#[derive(Default, Clone)]
pub(crate) struct FakeSource {
    pub events: Arc<Mutex<Option<mpsc::Sender<SourceEvent>>>>,
    pub commands: Arc<Mutex<Vec<SourceCommand>>>,
    pub started: Arc<Mutex<Vec<(String, String)>>>,
}

impl Source for FakeSource {
    fn start(&self, vm: &str, codec: &str) -> Result<SourceHandle> {
        let (tx, events) = mpsc::channel(64);
        *self.events.lock().unwrap() = Some(tx);
        self.started.lock().unwrap().push((vm.into(), codec.into()));
        let (commands, mut rx) = mpsc::unbounded_channel();
        let log = self.commands.clone();
        tokio::spawn(async move {
            while let Some(c) = rx.recv().await {
                log.lock().unwrap().push(c);
            }
        });
        Ok(SourceHandle { events, commands })
    }
}

impl FakeSource {
    pub async fn push(&self, e: SourceEvent) {
        let tx = self.events.lock().unwrap().clone().unwrap();
        tx.send(e).await.unwrap();
    }

    pub async fn ready(&self, backend: &str) {
        self.push(SourceEvent::Event(
            json!({"event":"ready","vm":"linux","backend":backend,"codec":"hevc"}),
        ))
        .await;
        self.push(SourceEvent::Event(json!({"event":"encoder","detail":{
            "event":"view","track":0,"x":0,"y":0,"w":1920,"h":1080,
            "src_w":1920,"src_h":1080,"out_w":1920,"out_h":1080,"bitrate":8000000}})))
            .await;
    }

    /// Encoder lines sent so far, oldest first; clears the log.
    pub async fn lines(&self) -> Vec<String> {
        settle().await;
        self.commands
            .lock()
            .unwrap()
            .drain(..)
            .filter_map(|c| match c {
                SourceCommand::Encoder(l) => Some(l),
                SourceCommand::Input(_) => None,
            })
            .collect()
    }
}

pub async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(5)).await;
}

/// A keyframe access unit: HEVC VPS, SPS (with `sps` as payload), IDR slice.
pub fn hevc_key(sps: u8) -> Vec<u8> {
    let mut v = vec![0, 0, 0, 1, 0x40, 1, 0xaa];
    v.extend([0, 0, 0, 1, 0x42, 1, sps]);
    v.extend([0, 0, 1, 0x26, 1, 0xbb]);
    v
}

fn crop(x: u16, y: u16, w: u16, h: u16) -> Option<Crop> {
    Some(Crop {
        x,
        y,
        w,
        h,
        out_w: 1170,
        out_h: 2532,
    })
}

#[test]
fn sps_is_found_per_codec() {
    let au = hevc_key(7);
    assert_eq!(first_sps(&au, true), Some(&[0x42, 1, 7][..]));
    // In H.264 terms none of these NAL headers is type 7.
    assert_eq!(first_sps(&au, false), None);
    let h264 = [
        0, 0, 0, 1, 0x67, 0x4d, 0, 0, 0, 1, 0x68, 0xee, 0, 0, 1, 0x65, 0x88,
    ];
    assert_eq!(first_sps(&h264, false), Some(&[0x67, 0x4d][..]));
}

#[test]
fn crop_controls_parse_and_normalize() {
    assert_eq!(Crop::from_control(&json!(null)), Some(None));
    assert_eq!(Crop::from_control(&json!({"x":1})), None);
    let c = Crop::from_control(&json!({"x":1900,"y":0,"w":999,"h":101,"out_w":10,"out_h":10}))
        .unwrap()
        .unwrap();
    let n = normalize(c, Some((1920, 1080))).unwrap();
    assert_eq!((n.x, n.w, n.h), (1900, 20, 100));
    assert_eq!(
        normalize(crop(0, 0, 1920, 1080).unwrap(), Some((1920, 1080))),
        None
    );
    assert_eq!(normalize(crop(10, 10, 100, 100).unwrap(), None), None);
}

#[tokio::test]
async fn viewers_share_one_encoder_and_crop_tracks() {
    let source = FakeSource::default();
    let hub = Hub::start(&source, "linux", "hevc", 3).unwrap();
    source.ready("apple-virtualization").await;
    let a = hub.join();
    let b = hub.join();
    let c = hub.join();
    assert_eq!(source.started.lock().unwrap().len(), 1, "one encoder");
    // Joins ask for one keyframe on track 0 (rate limited).
    assert_eq!(source.lines().await, vec!["keyframe"]);

    let now = Instant::now();
    hub.view(a.id, crop(640, 360, 960, 540), now);
    let lines = source.lines().await;
    assert_eq!(
        lines,
        vec![
            "track add 1",
            "@1 crop 640 360 960 540",
            "@1 scale 960 540",
            "@1 keyframe"
        ]
    );
    // The same rectangle shares the track; no new encoder session.
    hub.view(b.id, crop(640, 360, 960, 540), now);
    assert!(!source
        .lines()
        .await
        .iter()
        .any(|l| l.starts_with("track add")));
    // A third rectangle takes track 2; a fourth finds the limit (3 tracks).
    hub.view(c.id, crop(0, 0, 400, 300), now);
    assert!(source.lines().await.contains(&"track add 2".to_string()));
    let mut d = hub.join();
    hub.view(d.id, crop(8, 8, 200, 200), now);
    settle().await;
    let mut fallback = false;
    while let Ok(e) = d.events.try_recv() {
        fallback |= e["fallback"] == "full";
    }
    assert!(fallback, "no free track: full screen, zoom in the browser");

    // Frames reach only the viewers on their track, with that track's view.
    source
        .push(SourceEvent::Event(json!({"event":"encoder","detail":{
            "event":"view","track":1,"x":640,"y":360,"w":960,"h":540,
            "src_w":1920,"src_h":1080,"out_w":960,"out_h":540}})))
        .await;
    source
        .push(SourceEvent::Frame(EncodedFrame {
            track: 1,
            pts_us: 5,
            keyframe: true,
            annex_b: hevc_key(1),
        }))
        .await;
    settle().await;
    let fa = a.pacer.lock().unwrap().pop().expect("a on track 1");
    let fb = b.pacer.lock().unwrap().pop().expect("b on track 1");
    assert!(c.pacer.lock().unwrap().pop().is_none());
    assert_eq!(fa.bytes, fb.bytes);
    let (h, rest) = VideoHeader::decode(&fa.bytes).unwrap();
    assert_eq!((h.frame_seq, h.keyframe, h.sps_changed), (1, true, true));
    assert_eq!((h.view.x, h.view.w, h.view.src_w), (640, 960, 1920));
    assert_eq!(rest, &hevc_key(1)[..]);

    // The last viewer off a crop track removes it.
    hub.view(a.id, None, now);
    assert!(!source
        .lines()
        .await
        .iter()
        .any(|l| l.starts_with("track remove")));
    hub.leave(b.id);
    assert!(source.lines().await.contains(&"track remove 1".to_string()));
}

#[tokio::test]
async fn qemu_keeps_one_full_screen_track() {
    let source = FakeSource::default();
    let hub = Hub::start(&source, "linux", "hevc", 4).unwrap();
    source.ready("qemu-hvf").await;
    let mut a = hub.join();
    source.lines().await;
    hub.view(a.id, crop(10, 10, 300, 300), Instant::now());
    assert!(!source.lines().await.iter().any(|l| l.starts_with("track")));
    settle().await;
    let mut fallback = false;
    while let Ok(e) = a.events.try_recv() {
        fallback |= e["fallback"] == "full";
    }
    assert!(fallback);
}

#[tokio::test]
async fn track_bitrate_is_the_median_and_ignores_small_changes() {
    let source = FakeSource::default();
    let hub = Hub::start(&source, "linux", "hevc", 4).unwrap();
    source.ready("apple-virtualization").await;
    let (a, b, c) = (hub.join(), hub.join(), hub.join());
    source.lines().await;
    hub.bitrate(a.id, 1_000_000);
    hub.bitrate(b.id, 4_000_000);
    hub.bitrate(c.id, 9_000_000);
    assert_eq!(
        source.lines().await,
        vec!["bitrate 1000000", "bitrate 4000000"],
        "one, then the median of two (upper), then of three is still 4M"
    );
    hub.bitrate(c.id, 9_100_000);
    hub.bitrate(b.id, 4_100_000);
    assert!(
        source.lines().await.is_empty(),
        "under 5% is not worth a line"
    );
}

#[tokio::test]
async fn a_slow_viewer_drops_to_keyframes_and_asks_for_one() {
    let source = FakeSource::default();
    let hub = Hub::start(&source, "linux", "hevc", 4).unwrap();
    source.ready("apple-virtualization").await;
    let a = hub.join();
    source.lines().await;
    let frame = |key: bool| {
        SourceEvent::Frame(EncodedFrame {
            track: 0,
            pts_us: 1,
            keyframe: key,
            annex_b: if key {
                hevc_key(1)
            } else {
                vec![0, 0, 1, 2, 1, 0xcc]
            },
        })
    };
    source.push(frame(true)).await;
    settle().await;
    assert!(a.pacer.lock().unwrap().pop().unwrap().keyframe);
    // Two deltas while the viewer sends nothing: both dropped, keyframe asked.
    source.push(frame(false)).await;
    source.push(frame(false)).await;
    // Past the limiter's interval since the join request.
    tokio::time::sleep(vm_remote_proto::pacer::KEYFRAME_INTERVAL).await;
    source.push(frame(false)).await;
    let lines = source.lines().await;
    assert!(a.pacer.lock().unwrap().need_keyframe());
    assert!(a.pacer.lock().unwrap().pop().is_none());
    hub.poll(Instant::now());
    let lines = [lines, source.lines().await].concat();
    assert!(lines.contains(&"keyframe".to_string()), "{lines:?}");
}

async fn next(events: &mut mpsc::Receiver<SourceEvent>) -> SourceEvent {
    tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .unwrap()
        .unwrap()
}

/// A daemon that serves one `desktop.stream`: it checks the request, sends
/// a relay event and a frame, waits for one control message, and ends.
fn fake_daemon(dir: &std::path::Path) -> std::thread::JoinHandle<(Value, Value)> {
    use std::io::{BufRead, BufReader, Write};
    use veynodevm_api::{frame, Channel};
    let listener = std::os::unix::net::UnixListener::bind(dir.join("control.sock")).unwrap();
    std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(socket.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let hello = json!({"api":1,"daemon":env!("CARGO_PKG_VERSION"),"methods":["desktop.stream"],"home":null,
            "runtime":{"appleVirtualization":true,"qemu":false}});
        writeln!(socket, "{}", json!({ "result": hello })).unwrap();
        line.clear();
        reader.read_line(&mut line).unwrap();
        let request: Value = serde_json::from_str(&line).unwrap();
        writeln!(socket, "{}", json!({"result":{"stream":true}})).unwrap();
        let record = |kind: u8, payload: &[u8]| {
            let mut r = ((payload.len() + 1) as u32).to_be_bytes().to_vec();
            r.push(kind);
            r.extend_from_slice(payload);
            r
        };
        let mut video = 7u64.to_be_bytes().to_vec();
        video.push(1);
        video.extend_from_slice(&[0, 0, 0, 1, 0x40]);
        let mut track = vec![2u8];
        track.extend_from_slice(&video);
        // One record split across frames, then two in one frame.
        let ready = record(1, br#"{"event":"ready"}"#);
        frame::write_frame(&mut socket, Channel::Stdout, &ready[..3]).unwrap();
        frame::write_frame(&mut socket, Channel::Stdout, &ready[3..]).unwrap();
        let both = [record(2, &video), record(10, &track)].concat();
        frame::write_frame(&mut socket, Channel::Stdout, &both).unwrap();
        let (channel, payload) = frame::read_frame(&mut reader).unwrap().unwrap();
        assert_eq!(channel, Channel::Control as u8);
        let control: Value = serde_json::from_slice(&payload).unwrap();
        frame::write_frame(&mut socket, Channel::End, br#"{"result":{"exitCode":0}}"#).unwrap();
        (request, control)
    })
}

#[tokio::test]
async fn the_desktop_source_is_the_daemons_desktop_stream() {
    let dir = tempfile::tempdir().unwrap();
    let daemon = fake_daemon(dir.path());
    let source = DesktopStream {
        client: veynodevm_api::Client::at_socket(dir.path().join("control.sock"), "hub-test"),
    };
    let mut handle = source.start("web", "h264").unwrap();
    match next(&mut handle.events).await {
        SourceEvent::Event(v) => assert_eq!(v, json!({"event":"ready"})),
        other => panic!("{other:?}"),
    }
    for track in [0, 2] {
        match next(&mut handle.events).await {
            SourceEvent::Frame(f) => {
                assert_eq!((f.track, f.pts_us, f.keyframe), (track, 7, true));
                assert_eq!(f.annex_b, [0, 0, 0, 1, 0x40]);
            }
            other => panic!("{other:?}"),
        }
    }
    handle
        .commands
        .send(SourceCommand::Encoder("@0 keyframe".into()))
        .unwrap();
    match next(&mut handle.events).await {
        SourceEvent::Ended(message) => assert_eq!(message, "desktop relay ended"),
        other => panic!("{other:?}"),
    }
    let (request, control) = daemon.join().unwrap();
    assert_eq!(
        request,
        json!({"method":"desktop.stream","arguments":{"name":"web","codec":"h264","tracks":true}})
    );
    assert_eq!(control, json!({"encoder":"@0 keyframe"}));
}

#[tokio::test]
async fn a_daemon_that_is_not_there_ends_the_source_with_why() {
    let dir = tempfile::tempdir().unwrap();
    let source = DesktopStream {
        client: veynodevm_api::Client::at_socket(dir.path().join("control.sock"), "hub-test"),
    };
    let mut handle = source.start("web", "hevc").unwrap();
    match tokio::time::timeout(Duration::from_secs(5), handle.events.recv())
        .await
        .unwrap()
        .unwrap()
    {
        SourceEvent::Ended(message) => {
            assert!(message.starts_with("desktop.stream: "), "{message}")
        }
        other => panic!("{other:?}"),
    }
}
