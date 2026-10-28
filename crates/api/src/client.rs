//! A blocking client: one connection per call, `hello` first.
use crate::{
    envelope::{open, ApiError, ErrorCode, Hello},
    frame::{read_frame, read_line, write_chunked, write_frame, write_line, Channel},
    line_limit, socket, API, MAX_LINE,
};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{
    io::BufReader,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

/// How long a call waits for its answer unless told otherwise.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(40);

/// What a client says when the daemon running is not the one it was built with.
pub const RESTART: &str =
    "restart the daemon: `veynodevm daemon stop`, then `veynodevm daemon start`";

/// The daemon at one home, as one named client.
#[derive(Clone, Debug)]
pub struct Client {
    socket: PathBuf,
    name: String,
}

impl Client {
    /// The daemon under `home`; `name` is this client, such as `veynodevm-cli/0.1.0`.
    pub fn new(home: &Path, name: impl Into<String>) -> Self {
        Self {
            socket: socket(home),
            name: name.into(),
        }
    }

    /// The daemon at the shared home ([`crate::home`]).
    pub fn from_env(name: impl Into<String>) -> Result<Self> {
        Ok(Self::new(&crate::home()?, name))
    }

    /// The daemon at an explicit socket path.
    pub fn at_socket(socket: PathBuf, name: impl Into<String>) -> Self {
        Self {
            socket,
            name: name.into(),
        }
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// A connection that has said `hello`, and the daemon's answer.
    fn connect(&self) -> Result<(UnixStream, BufReader<UnixStream>, Hello)> {
        let mut stream = UnixStream::connect(&self.socket).with_context(|| {
            format!(
                "connect to the daemon at {}; start it with `veynodevm daemon start`",
                self.socket.display()
            )
        })?;
        stream.set_write_timeout(Some(Duration::from_secs(15)))?;
        stream.set_read_timeout(Some(Duration::from_secs(15)))?;
        write_line(
            &mut stream,
            &json!({"method":"hello","arguments":{"api":API,"client":self.name}}),
        )?;
        let mut reader = BufReader::new(stream.try_clone()?);
        let line = read_line(&mut reader, MAX_LINE)?.context("the daemon closed the connection")?;
        let hello = open(serde_json::from_slice(&line)?).map_err(|e| self.refused_hello(e))?;
        Ok((stream, reader, serde_json::from_value(hello)?))
    }

    /// A refused `hello` is a daemon of another API: one older than API 1
    /// refuses the method itself, without a code.
    fn refused_hello(&self, error: ApiError) -> ApiError {
        let message = if error.code == ErrorCode::Version {
            format!("{}; {RESTART}", error.message)
        } else {
            format!(
                "the daemon at {} does not speak API {API} (it answered hello with: {}); {RESTART}",
                self.socket.display(),
                error.message
            )
        };
        ApiError {
            code: ErrorCode::Version,
            message,
        }
    }

    /// The daemon's `hello`.
    pub fn hello(&self) -> Result<Hello> {
        Ok(self.connect()?.2)
    }

    /// A method that answers once, within [`DEFAULT_TIMEOUT`].
    pub fn call(&self, method: &str, args: Value) -> Result<Value> {
        self.call_within(method, args, DEFAULT_TIMEOUT)
    }

    /// A method that answers once, within `timeout`. A refusal comes back as
    /// an [`ApiError`] (`anyhow::Error::downcast_ref` reads its code).
    pub fn call_within(&self, method: &str, args: Value, timeout: Duration) -> Result<Value> {
        let (mut stream, mut reader, hello) = self.connect()?;
        stream.set_read_timeout(Some(timeout))?;
        write_line(&mut stream, &json!({"method":method,"arguments":args}))?;
        let line = read_line(&mut reader, line_limit(method))?
            .with_context(|| format!("the daemon closed the connection during {method}"))?;
        Ok(open(serde_json::from_slice(&line)?).map_err(|e| unknown_method(e, method, &hello))?)
    }

    /// A streaming method, started: its frames come from [`Stream::next_event`].
    pub fn stream(&self, method: &str, args: Value) -> Result<Stream> {
        let (mut stream, mut reader, hello) = self.connect()?;
        write_line(&mut stream, &json!({"method":method,"arguments":args}))?;
        let line = read_line(&mut reader, MAX_LINE)?
            .with_context(|| format!("the daemon closed the connection during {method}"))?;
        let started =
            open(serde_json::from_slice(&line)?).map_err(|e| unknown_method(e, method, &hello))?;
        if started.get("stream") != Some(&Value::Bool(true)) {
            return Err(ApiError {
                code: ErrorCode::Failed,
                message: format!("{method} did not stream"),
            }
            .into());
        }
        stream.set_read_timeout(None)?;
        reader.get_ref().set_read_timeout(None)?;
        stream.set_write_timeout(None)?;
        Ok(Stream {
            reader,
            sender: StreamSender(Arc::new(Mutex::new(stream))),
        })
    }
}

/// A method the daemon does not know is most often a daemon older than
/// this client, left running across an upgrade.
fn unknown_method(error: ApiError, method: &str, hello: &Hello) -> ApiError {
    if error.message != format!("unknown method {method}") {
        return error;
    }
    let ours = env!("CARGO_PKG_VERSION");
    let message = if hello.daemon == ours {
        error.message
    } else {
        format!(
            "{}: the daemon is version {}, this client {ours}; {RESTART}",
            error.message, hello.daemon
        )
    };
    ApiError { message, ..error }
}

/// What a stream delivers.
#[derive(Debug)]
pub enum Event {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    /// An `event` frame's JSON.
    Event(Value),
    /// The call's answer; the stream is over.
    End(Result<Value, ApiError>),
}

/// A started streaming call.
pub struct Stream {
    reader: BufReader<UnixStream>,
    sender: StreamSender,
}

impl Stream {
    /// The next frame from the daemon. A channel this client does not read
    /// is skipped; the connection closing before `end` is an error.
    pub fn next_event(&mut self) -> Result<Event> {
        loop {
            let (channel, payload) = read_frame(&mut self.reader)?
                .context("the daemon closed the stream before its end")?;
            return Ok(match Channel::from_u8(channel) {
                Some(Channel::Stdout) => Event::Stdout(payload),
                Some(Channel::Stderr) => Event::Stderr(payload),
                Some(Channel::Event) => Event::Event(serde_json::from_slice(&payload)?),
                Some(Channel::End) => Event::End(open(serde_json::from_slice(&payload)?)),
                _ => continue,
            });
        }
    }

    /// A handle that writes to this stream from another thread.
    pub fn sender(&self) -> StreamSender {
        self.sender.clone()
    }

    /// Reads to the end, handing every frame before it to `each`.
    pub fn finish(mut self, mut each: impl FnMut(Event) -> Result<()>) -> Result<Value> {
        loop {
            match self.next_event()? {
                Event::End(result) => return Ok(result?),
                event => each(event)?,
            }
        }
    }
}

/// The writing half of a stream.
#[derive(Clone)]
pub struct StreamSender(Arc<Mutex<UnixStream>>);

impl StreamSender {
    /// Input for the call, in as many frames as it takes.
    pub fn stdin(&self, data: &[u8]) -> Result<()> {
        write_chunked(&mut *self.0.lock().unwrap(), Channel::Stdin, data)
    }
    /// The end of the input.
    pub fn close_stdin(&self) -> Result<()> {
        write_frame(&mut *self.0.lock().unwrap(), Channel::Stdin, b"")
    }
    /// A `control` message.
    pub fn control(&self, message: &Value) -> Result<()> {
        write_frame(
            &mut *self.0.lock().unwrap(),
            Channel::Control,
            &serde_json::to_vec(message)?,
        )
    }
    /// `TERM`, `INT`, `HUP` or `KILL` for what the call runs.
    pub fn signal(&self, name: &str) -> Result<()> {
        self.control(&json!({ "signal": name }))
    }
    /// Ends the call: the daemon cancels it.
    pub fn cancel(&self) {
        let _ = self.0.lock().unwrap().shutdown(std::net::Shutdown::Both);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::BufRead, os::unix::net::UnixListener, thread};

    /// A daemon that answers each connection's lines with `answers`, in order.
    fn daemon(answers: Vec<Value>) -> (tempfile::TempDir, Client, thread::JoinHandle<()>) {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("control.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let served = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            for answer in answers {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 {
                    return;
                }
                write_line(&mut writer, &answer).unwrap();
            }
        });
        (dir, Client::at_socket(socket, "test/1"), served)
    }

    fn hello(daemon: &str) -> Value {
        json!({"result":{"api":API,"daemon":daemon,"methods":[],"home":null,"runtime":{"appleVirtualization":true,"qemu":false}}})
    }

    fn refusal(result: Result<Value>) -> ApiError {
        result.unwrap_err().downcast::<ApiError>().unwrap()
    }

    #[test]
    fn a_daemon_of_another_api_says_to_restart_it() {
        let (_dir, client, served) = daemon(vec![
            json!({"error":"this daemon serves API 2; send hello with api 2","code":"version"}),
        ]);
        let error = refusal(client.call("container.ls", json!({})));
        served.join().unwrap();
        assert_eq!(error.code, ErrorCode::Version);
        assert!(
            error.message.starts_with("this daemon serves API 2"),
            "{error}"
        );
        assert!(error.message.ends_with(RESTART), "{error}");
    }

    #[test]
    fn a_daemon_older_than_api_1_refuses_hello_without_a_code() {
        let (_dir, client, served) = daemon(vec![json!({"error":"unsupported operation: hello"})]);
        let error = refusal(client.call("container.ls", json!({})));
        served.join().unwrap();
        assert_eq!(error.code, ErrorCode::Version);
        assert!(error.message.contains("does not speak API 1"), "{error}");
        assert!(
            error.message.contains("unsupported operation: hello"),
            "{error}"
        );
        assert!(error.message.ends_with(RESTART), "{error}");
    }

    #[test]
    fn an_unknown_method_names_an_older_daemon() {
        let unknown = json!({"error":"unknown method machine.endpoints","code":"unsupported"});
        let (_dir, client, served) = daemon(vec![hello("0.0.9"), unknown.clone()]);
        let error = refusal(client.call("machine.endpoints", json!({"name":"a"})));
        served.join().unwrap();
        assert_eq!(error.code, ErrorCode::Unsupported);
        assert!(
            error.message.contains("the daemon is version 0.0.9"),
            "{error}"
        );
        assert!(error.message.ends_with(RESTART), "{error}");

        // The same version: the method is just unknown.
        let (_dir, client, served) = daemon(vec![hello(env!("CARGO_PKG_VERSION")), unknown]);
        let error = refusal(client.call("machine.endpoints", json!({"name":"a"})));
        served.join().unwrap();
        assert_eq!(error.message, "unknown method machine.endpoints");
    }
}
