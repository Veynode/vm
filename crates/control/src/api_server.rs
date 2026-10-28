//! Local API 1 on `control.sock` (`docs/api.md`): a connection whose first
//! line is `hello`. Namespaced methods (`machine.*`, `container.*`, …) are the
//! daemon's services and run on the connection's own thread without the
//! manager lock; every other method is the VM layer, dispatched as a legacy
//! request is and answered with a code.
use crate::{services, Manager};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{
    io::BufReader,
    os::unix::net::UnixStream,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use veynodevm_api::{
    answer,
    frame::{read_frame, read_line, write_chunked, write_frame, write_line},
    refusal, refuse, Channel, ErrorCode, Hello, Runtime, API, MAX_LINE,
};

/// The daemon's version, as `hello` reports it.
pub const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The VM layer's methods, as `hello` lists them.
pub const VM_METHODS: &[&str] = &[
    "list",
    "capabilities",
    "create",
    "events",
    "session_end",
    "status",
    "start",
    "force_stop",
    "shutdown",
    "delete",
    "log_read",
    "serial_write",
    "show_window",
    "close_window",
    "window_status",
    "windows",
    "guest_info",
    "control_lease",
    "guest_stream_status",
    "start_guest_stream_bridge",
    "stop_guest_stream_bridge",
    "guest_stream_bridge_binding",
    "exec",
    "exec_start",
    "exec_poll",
    "exec_cancel",
    "file_upload_begin",
    "file_upload_write",
    "file_upload_commit",
    "file_download_begin",
    "file_download_read",
    "file_cancel",
    "file_session_end",
    "screenshot",
    "frame_status",
    "screen_wait",
    "input_click",
    "input_key",
    "input_scroll",
    "input_drag",
    "input_text",
    "input_step",
    "input_text_step",
    "input_cleanup",
    "desktop_input",
    "desktop_state",
    "desktop_apps",
    "window_action",
    "app_launch",
    "accessibility_tree",
    "accessibility_find",
    "accessibility_action",
    "snapshot_create",
    "snapshot_list",
    "snapshot_delete",
    "snapshot_status",
    "snapshot_reconcile",
    "snapshot_resume_source",
    "snapshot_restore_start",
    "machine_state_save",
    "clock_sync",
    "driver_rpc",
];

/// What a service sees of the daemon.
pub struct Ctx {
    /// The daemon's state directory, where its socket is.
    pub state_dir: std::path::PathBuf,
    pub runtime: Runtime,
    /// The client's `hello` name.
    pub client: String,
}

impl Ctx {
    /// A VM-layer call, made to this daemon over its own socket so that it
    /// takes the manager lock, deadlines and deferred answers as any client's.
    pub fn vm(&self, method: &str, args: Value) -> Result<Value> {
        crate::call(&self.state_dir, method, args)
    }
}

fn hello_answer(manager: &Manager) -> Hello {
    let mut methods = vec!["hello".to_string()];
    methods.extend(services::names().map(str::to_string));
    methods.extend(VM_METHODS.iter().map(|m| m.to_string()));
    Hello {
        api: API,
        daemon: DAEMON_VERSION.into(),
        methods,
        home: crate::home::root()
            .ok()
            .map(|path| path.to_string_lossy().into_owned()),
        runtime: runtime(manager),
    }
}

fn runtime(manager: &Manager) -> Runtime {
    Runtime {
        apple_virtualization: manager.backend.is_file(),
        qemu: manager.qemu.is_some(),
    }
}

/// Serves one API 1 connection whose `hello` line has been read.
pub(crate) fn serve(
    mut stream: UnixStream,
    mut reader: BufReader<UnixStream>,
    manager: Arc<Mutex<Manager>>,
    hello: Value,
    admitted_at: Instant,
) -> Result<()> {
    if hello["arguments"]["api"] != json!(API) {
        return write_line(
            &mut stream,
            &refusal(&refuse(
                ErrorCode::Version,
                format!("this daemon serves API {API}; send hello with api {API}"),
            )),
        );
    }
    let client = hello["arguments"]["client"]
        .as_str()
        .unwrap_or("unnamed")
        .chars()
        .take(128)
        .collect::<String>();
    let (hello, ctx) = {
        let manager = manager.lock().unwrap();
        (
            hello_answer(&manager),
            Ctx {
                state_dir: manager.dir.clone(),
                runtime: runtime(&manager),
                client,
            },
        )
    };
    write_line(&mut stream, &json!({ "result": hello }))?;
    let Some(line) = read_line(&mut reader, MAX_LINE)? else {
        return Ok(()); // `hello` alone: a probe.
    };
    let request: Value = match serde_json::from_slice(&line) {
        Ok(request) => request,
        Err(e) => {
            return write_line(
                &mut stream,
                &refusal(&refuse(
                    ErrorCode::Invalid,
                    format!("request is not JSON: {e}"),
                )),
            )
        }
    };
    let Some(method) = request["method"].as_str().map(str::to_string) else {
        return write_line(
            &mut stream,
            &refusal(&refuse(ErrorCode::Invalid, "request has no method")),
        );
    };
    let args = match request.get("arguments") {
        None | Some(Value::Null) => json!({}),
        Some(args) => args.clone(),
    };
    match services::lookup(&method) {
        Some(services::Handler::Call(call)) => {
            let result = call(&ctx, args);
            write_line(&mut stream, &answer(&result))
        }
        Some(services::Handler::Stream(run)) => {
            write_line(&mut stream, &json!({"result":{"stream":true}}))?;
            let io = StreamIo::start(stream, reader)?;
            let result = run(&ctx, args, &io);
            io.end(&result)
        }
        None if method == "native_attach" => {
            crate::native_control::serve(stream, manager, request, admitted_at)
        }
        None if !VM_METHODS.contains(&method.as_str()) => write_line(
            &mut stream,
            &refusal(&refuse(
                ErrorCode::Unsupported,
                format!("unknown method {method}"),
            )),
        ),
        None => {
            let result = crate::vm_result(&stream, &manager, &request, admitted_at);
            crate::write_vm_reply(&mut stream, &method, result, admitted_at, true)
        }
    }
}

/// A stream's `stdout` channel, as [`std::io::Write`].
#[derive(Clone)]
pub struct Output(Arc<Mutex<UnixStream>>);

impl std::io::Write for Output {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        write_chunked(&mut *self.0.lock().unwrap(), Channel::Stdout, data)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::BrokenPipe, e))?;
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// [`StreamIo::reader`].
pub struct InputReader<'a> {
    io: &'a StreamIo,
    pending: Vec<u8>,
}

impl std::io::Read for InputReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        while self.pending.is_empty() {
            match self.io.stdin(Duration::from_secs(1)) {
                Input::Data(data) => self.pending = data,
                Input::Idle => {}
                Input::Eof | Input::Closed => return Ok(0),
            }
        }
        let n = buffer.len().min(self.pending.len());
        buffer[..n].copy_from_slice(&self.pending[..n]);
        self.pending.drain(..n);
        Ok(n)
    }
}

/// What came on a stream's `stdin` channel.
#[derive(Debug, PartialEq, Eq)]
pub enum Input {
    Data(Vec<u8>),
    /// The client ended its input (an empty frame).
    Eof,
    /// Nothing within the wait.
    Idle,
    /// The client went away.
    Closed,
}

/// A streaming call's side of the connection. A reader thread sorts the
/// client's frames into `stdin` and `control` and notices the client leaving,
/// which a handler checks with [`StreamIo::closed`] and treats as cancellation.
pub struct StreamIo {
    writer: Arc<Mutex<UnixStream>>,
    stdin: Mutex<Receiver<Vec<u8>>>,
    control: Mutex<Receiver<Value>>,
    closed: Arc<AtomicBool>,
    stdin_ended: AtomicBool,
}

impl StreamIo {
    fn start(stream: UnixStream, mut reader: BufReader<UnixStream>) -> Result<Self> {
        reader.get_ref().set_read_timeout(None)?;
        stream.set_write_timeout(Some(Duration::from_secs(60)))?;
        let (stdin_tx, stdin) = mpsc::sync_channel(16);
        let (control_tx, control) = mpsc::sync_channel(64);
        let closed = Arc::new(AtomicBool::new(false));
        let flag = closed.clone();
        thread::spawn(move || {
            // A handler that reads no input drops nothing it needs; keep
            // reading so the close is still seen.
            while let Ok(Some((channel, payload))) = read_frame(&mut reader) {
                match Channel::from_u8(channel) {
                    Some(Channel::Stdin) => {
                        let _ = stdin_tx.send(payload);
                    }
                    Some(Channel::Control) => {
                        if let Ok(message) = serde_json::from_slice::<Value>(&payload) {
                            let _ = control_tx.send(message);
                        }
                    }
                    _ => {}
                }
            }
            flag.store(true, Ordering::SeqCst);
        });
        Ok(Self {
            writer: Arc::new(Mutex::new(stream)),
            stdin: Mutex::new(stdin),
            control: Mutex::new(control),
            closed,
            stdin_ended: AtomicBool::new(false),
        })
    }

    /// Whether the client has gone; a handler stops what it runs when it has.
    pub fn closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    pub fn stdout(&self, data: &[u8]) -> Result<()> {
        write_chunked(&mut *self.writer.lock().unwrap(), Channel::Stdout, data)
    }
    pub fn stderr(&self, data: &[u8]) -> Result<()> {
        write_chunked(&mut *self.writer.lock().unwrap(), Channel::Stderr, data)
    }
    pub fn event(&self, event: &Value) -> Result<()> {
        write_frame(
            &mut *self.writer.lock().unwrap(),
            Channel::Event,
            &serde_json::to_vec(event)?,
        )
    }
    pub fn progress(&self, message: &str) -> Result<()> {
        self.event(&json!({ "progress": message }))
    }

    /// An owned writer to the `stdout` channel, for threads a handler starts.
    pub fn output(&self) -> Output {
        Output(self.writer.clone())
    }

    /// The next input, waiting up to `wait`.
    pub fn stdin(&self, wait: Duration) -> Input {
        if self.stdin_ended.load(Ordering::SeqCst) {
            return Input::Eof;
        }
        match self.stdin.lock().unwrap().recv_timeout(wait) {
            Ok(data) if data.is_empty() => {
                self.stdin_ended.store(true, Ordering::SeqCst);
                Input::Eof
            }
            Ok(data) => Input::Data(data),
            Err(RecvTimeoutError::Timeout) => Input::Idle,
            Err(RecvTimeoutError::Disconnected) => Input::Closed,
        }
    }

    /// The `stdin` channel as [`std::io::Read`]: its end, or the client
    /// leaving, reads as the end of input.
    pub fn reader(&self) -> InputReader<'_> {
        InputReader {
            io: self,
            pending: Vec::new(),
        }
    }

    /// A flag that turns true once the client has gone, for threads a handler starts.
    pub fn gone(&self) -> Arc<AtomicBool> {
        self.closed.clone()
    }

    /// The next `control` message, waiting up to `wait`.
    pub fn control(&self, wait: Duration) -> Option<Value> {
        self.control.lock().unwrap().recv_timeout(wait).ok()
    }

    fn end(self, result: &Result<Value>) -> Result<()> {
        let payload = serde_json::to_vec(&answer(result))?;
        write_frame(&mut *self.writer.lock().unwrap(), Channel::End, &payload)
            .context("write the stream's end")
    }
}
