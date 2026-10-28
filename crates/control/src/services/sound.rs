//! A running QEMU machine's sound and microphone endpoints, in Veynode
//! core's format (docs/api.md, `machine.sound` and `machine.microphone`).
//!
//! Each is a loopback TCP listener like `machine.display`'s (the same epoch,
//! token and newest-viewer rules, `display::endpoint`). After the token and a
//! newline, the sound viewer reads raw s16le 48 kHz stereo PCM for as long as
//! it stays connected, and the microphone viewer writes raw s16le 48 kHz mono
//! PCM. Both ride one connection to the VM's SPICE console broker per
//! machine, which asked `audio_start`: the broker plays the Guest's sound to
//! the client that asked last and takes the microphone from it only.
//! Apple Virtualization machines have no VeynodeVM sound relay.
use super::{display, machines::base64, machines::Machine};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io::{BufReader, Read, Write},
    net::{Shutdown, TcpStream},
    os::unix::net::UnixStream,
    sync::{Arc, Mutex, Weak},
    thread,
    time::Duration,
};
use veynodevm_api::{refuse, ErrorCode};

/// The endpoints' sample rate.
const RATE: u32 = 48_000;
/// 10 ms of 48 kHz mono s16le: one microphone frame to the broker.
const MIC_FRAME: usize = 960;

/// `machine.sound`: what the Guest plays, 48 kHz stereo s16le.
pub fn sound(machine: &Machine, epoch: u64) -> Result<Value> {
    let socket = console(machine)?;
    display::endpoint(
        &machine.name,
        "sound",
        epoch,
        json!({"format":"pcm","rate":RATE,"channels":2}),
        Arc::new(move |name, stream| hear(name, &socket, stream)),
    )
}

/// `machine.microphone`: what the Guest records, 48 kHz mono s16le.
pub fn microphone(machine: &Machine, epoch: u64) -> Result<Value> {
    let socket = console(machine)?;
    display::endpoint(
        &machine.name,
        "microphone",
        epoch,
        json!({"format":"pcm","rate":RATE,"channels":1}),
        Arc::new(move |name, stream| speak(name, &socket, stream)),
    )
}

/// The VM's console broker socket, refusing machines without a sound card.
fn console(machine: &Machine) -> Result<String> {
    if machine.backend != "qemu-hvf" {
        return Err(refuse(
            ErrorCode::Unsupported,
            format!(
                "{} runs on Apple Virtualization, which has no VeynodeVM sound relay",
                machine.name
            ),
        ));
    }
    let status = crate::call(
        &crate::home::state_dir()?,
        "status",
        json!({"id":machine.name}),
    )?;
    if !super::machines::running(status["state"].as_str().unwrap_or_default()) {
        return Err(refuse(
            ErrorCode::State,
            format!("{} is not running", machine.name),
        ));
    }
    if status["config"]["audio"] != true {
        return Err(refuse(
            ErrorCode::Unsupported,
            format!(
                "{} has no sound card (machine.update with audio, while stopped)",
                machine.name
            ),
        ));
    }
    match status["runtime"]["console_socket"].as_str() {
        Some(socket) => Ok(socket.to_owned()),
        None => bail!("{} has no console broker", machine.name),
    }
}

/// One machine's broker connection, shared by its sound and microphone viewers.
struct Link {
    broker: Mutex<UnixStream>,
    /// The sound viewer, which the Guest's PCM goes to.
    speaker: Mutex<Option<TcpStream>>,
}

impl Drop for Link {
    fn drop(&mut self) {
        // The reader thread ends with the connection.
        let _ = self.broker.lock().unwrap().shutdown(Shutdown::Both);
    }
}

/// Links by machine name, held only by their viewers.
static LINKS: Mutex<Option<HashMap<String, Weak<Link>>>> = Mutex::new(None);

/// The machine's link (the running one, or a new connection), which asks the
/// broker for audio again: a browser desktop may have taken it since.
fn link(name: &str, socket: &str) -> Result<Arc<Link>> {
    let link = shared(name, socket)?;
    link.broker
        .lock()
        .unwrap()
        .write_all(b"{\"request_id\":\"audio\",\"method\":\"audio_start\"}\n")
        .context("the console broker closed")?;
    Ok(link)
}

fn shared(name: &str, socket: &str) -> Result<Arc<Link>> {
    let mut links = LINKS.lock().unwrap();
    let links = links.get_or_insert_with(HashMap::new);
    if let Some(link) = links.get(name).and_then(Weak::upgrade) {
        return Ok(link);
    }
    let broker = UnixStream::connect(socket).context("connecting to the console broker")?;
    let link = Arc::new(Link {
        broker: Mutex::new(broker.try_clone()?),
        speaker: Mutex::new(None),
    });
    let (reader, weak) = (broker, Arc::downgrade(&link));
    let label = name.to_owned();
    thread::spawn(move || {
        if let Err(error) = pump(reader, &weak) {
            crate::log_line(&format!("sound of {label}: {error:#}"));
        }
    });
    // The broker ends a connection that sends nothing for 300 s (a muted microphone).
    let keepalive = Arc::downgrade(&link);
    thread::spawn(move || loop {
        thread::sleep(Duration::from_secs(60));
        let Some(link) = keepalive.upgrade() else {
            return;
        };
        let line = b"{\"request_id\":\"keepalive\",\"method\":\"status\"}\n";
        if link.broker.lock().unwrap().write_all(line).is_err() {
            return;
        }
    });
    links.insert(name.to_owned(), Arc::downgrade(&link));
    Ok(link)
}

/// The broker's records until it closes: audio (kind 3) to the speaker, as
/// 48 kHz stereo; a refusal (kind 1 with an error) ends the link.
fn pump(broker: UnixStream, link: &Weak<Link>) -> Result<()> {
    let mut from = BufReader::new(broker);
    let mut resampler = Resampler::default();
    loop {
        let mut header = [0u8; 13];
        if let Err(error) = from.read_exact(&mut header) {
            // The last viewer's end closes the connection: nothing went wrong.
            return match link.upgrade() {
                None => Ok(()),
                Some(_) => Err(error.into()),
            };
        }
        let size = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
        if size == 0 || size > 12 * 1024 * 1024 {
            bail!("console record size {size} outside bound")
        }
        let mut body = vec![0; size];
        from.read_exact(&mut body)?;
        let Some(link) = link.upgrade() else {
            return Ok(());
        };
        match header[4] {
            3 => {
                let pcm = resampler.stereo_48k(&body)?;
                let mut speaker = link.speaker.lock().unwrap();
                if let Some(viewer) = speaker.as_mut() {
                    if viewer.write_all(&pcm).is_err() {
                        let _ = viewer.shutdown(Shutdown::Both);
                        *speaker = None;
                    }
                }
            }
            1 => {
                let reply: Value = serde_json::from_slice(&body)?;
                if reply["request_id"] == "audio" && !reply["error"].is_null() {
                    bail!("the console broker refused audio: {}", reply["error"])
                }
            }
            _ => {}
        }
    }
}

/// Serves the sound viewer: PCM goes out from `pump` until the viewer closes.
fn hear(name: &str, socket: &str, stream: TcpStream) -> Result<()> {
    let link = link(name, socket)?;
    // A viewer that stops reading is cut rather than holding the broker back.
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    *link.speaker.lock().unwrap() = Some(stream.try_clone()?);
    // The viewer sends nothing; its end (or a cut) ends the read.
    let mut sink = [0u8; 256];
    let mut reader = &stream;
    while matches!(reader.read(&mut sink), Ok(n) if n > 0) {}
    let mut speaker = link.speaker.lock().unwrap();
    if speaker
        .as_ref()
        .is_some_and(|s| s.peer_addr().ok() == stream.peer_addr().ok())
    {
        *speaker = None;
    }
    Ok(())
}

/// Serves the microphone viewer: its PCM goes to the broker in 10 ms frames.
fn speak(name: &str, socket: &str, stream: TcpStream) -> Result<()> {
    let link = link(name, socket)?;
    let mut reader = BufReader::new(stream);
    let mut frame = vec![0u8; MIC_FRAME];
    loop {
        if reader.read_exact(&mut frame).is_err() {
            return Ok(());
        }
        let line = format!(
            "{{\"request_id\":\"mic\",\"method\":\"audio_in\",\"pcm\":\"{}\"}}\n",
            base64(&frame)
        );
        link.broker
            .lock()
            .unwrap()
            .write_all(line.as_bytes())
            .context("the console broker closed")?;
    }
}

/// Converts the broker's audio bodies (`u32 BE rate, u8 channels, s16le
/// PCM`) to 48 kHz stereo s16le, linearly, carrying the position across
/// bodies.
#[derive(Default)]
struct Resampler {
    rate: u32,
    /// Where the next output sample falls, in input frames past `last`.
    position: f64,
    /// The last input frame of the previous body.
    last: Option<(i16, i16)>,
}

impl Resampler {
    fn stereo_48k(&mut self, body: &[u8]) -> Result<Vec<u8>> {
        if body.len() < 5 {
            bail!("short audio record")
        }
        let rate = u32::from_be_bytes(body[..4].try_into().unwrap());
        let channels = body[4] as usize;
        let pcm = &body[5..];
        if !(8_000..=96_000).contains(&rate)
            || !(1..=2).contains(&channels)
            || !pcm.len().is_multiple_of(2 * channels)
        {
            bail!("malformed audio record ({rate} Hz, {channels} channels)")
        }
        let sample = |i: usize| i16::from_le_bytes([pcm[2 * i], pcm[2 * i + 1]]);
        let frames: Vec<(i16, i16)> = (0..pcm.len() / 2 / channels)
            .map(|f| (sample(f * channels), sample(f * channels + channels - 1)))
            .collect();
        let mut out = Vec::with_capacity(frames.len() * 4 * RATE as usize / rate as usize + 8);
        let mut put = |(l, r): (i16, i16)| {
            out.extend_from_slice(&l.to_le_bytes());
            out.extend_from_slice(&r.to_le_bytes());
        };
        if rate == RATE {
            frames.iter().copied().for_each(put);
            self.last = frames.last().copied();
            return Ok(out);
        }
        if self.rate != rate {
            (self.rate, self.position, self.last) = (rate, 0.0, None);
        }
        // Input frame -1 is the previous body's last, so bodies join smoothly.
        let at = |i: isize| match i {
            -1 => self.last.unwrap_or(frames[0]),
            i => frames[i as usize],
        };
        let step = f64::from(rate) / f64::from(RATE);
        let mut position = self.position - 1.0;
        while position < frames.len() as f64 - 1.0 {
            let base = position.floor();
            let t = position - base;
            let (a, b) = (at(base as isize), at(base as isize + 1));
            let mix = |x: i16, y: i16| (f64::from(x) + (f64::from(y) - f64::from(x)) * t) as i16;
            put((mix(a.0, b.0), mix(a.1, b.1)));
            position += step;
        }
        self.position = position - (frames.len() as f64 - 1.0);
        self.last = frames.last().copied();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(rate: u32, channels: u8, samples: &[i16]) -> Vec<u8> {
        let mut b = rate.to_be_bytes().to_vec();
        b.push(channels);
        samples
            .iter()
            .for_each(|s| b.extend_from_slice(&s.to_le_bytes()));
        b
    }

    fn samples(pcm: &[u8]) -> Vec<i16> {
        pcm.chunks(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect()
    }

    #[test]
    fn stereo_48k_passes_through_and_mono_doubles() {
        let mut r = Resampler::default();
        assert_eq!(
            samples(&r.stereo_48k(&body(48_000, 2, &[1, 2, 3, 4])).unwrap()),
            [1, 2, 3, 4]
        );
        assert_eq!(
            samples(&r.stereo_48k(&body(48_000, 1, &[5, -6])).unwrap()),
            [5, 5, -6, -6]
        );
    }

    #[test]
    fn other_rates_are_resampled_without_drift() {
        let mut r = Resampler::default();
        // 1 s of 44.1 kHz in 10 ms bodies comes out as 1 s of 48 kHz.
        let mut frames = 0;
        for _ in 0..100 {
            let ramp: Vec<i16> = (0..441).map(|i| i as i16).collect();
            frames += r.stereo_48k(&body(44_100, 1, &ramp)).unwrap().len() / 4;
        }
        assert!((47_990..=48_010).contains(&frames), "{frames}");
        // A ramp stays a ramp, between its ends.
        let mut r = Resampler::default();
        let ramp: Vec<i16> = (0..240).map(|i| i * 100).collect();
        let out = samples(&r.stereo_48k(&body(24_000, 1, &ramp)).unwrap());
        let left: Vec<i16> = out.iter().step_by(2).copied().collect();
        assert!(left.windows(2).all(|w| w[1] >= w[0]));
        assert!(*left.last().unwrap() <= 23_900);
    }

    #[test]
    fn malformed_records_are_refused() {
        let mut r = Resampler::default();
        assert!(r.stereo_48k(&body(48_000, 3, &[1, 2, 3])).is_err());
        assert!(r.stereo_48k(&body(1_000, 1, &[1])).is_err());
        assert!(r.stereo_48k(&[0, 0]).is_err());
        let mut odd = body(48_000, 2, &[1, 2]);
        odd.push(0);
        assert!(r.stereo_48k(&odd).is_err());
    }
}
