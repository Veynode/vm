//! The machine methods as core's plugin calls them (`packages/plugins/veynodevm`
//! in core; its `tests/helpers/fake-daemon.ts` is the reference for the
//! shapes), against a daemon with no worker: everything a stopped machine does.
use crate::{handle_connection, resources::MemoryBudget, Manager};
use serde_json::{json, Value};
use std::{
    fs,
    os::unix::net::UnixListener,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
    thread,
    time::Instant,
};
use veynodevm_api::{ApiError, Client, ErrorCode, Event};

/// One home for the process: services read `VEYNODEVM_HOME`, which tests
/// share, so each test names its own machines.
fn home() -> &'static PathBuf {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap().keep();
        let dir = crate::test_private_state(&dir);
        std::env::set_var("VEYNODEVM_HOME", &dir);
        dir
    })
}

fn daemon() -> Client {
    let home = home();
    let state = crate::home::private_dir(&home.join("state")).unwrap();
    let state = state.join(format!("d{}", crate::services::util::random_hex(4)));
    crate::home::private_dir(&state).unwrap();
    let socket = state.join("control.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let manager = Manager::load(
        state,
        "/unused".into(),
        MemoryBudget::new(16 * 1024 * 1024 * 1024, None).unwrap(),
    )
    .unwrap();
    let manager = Arc::new(Mutex::new(manager));
    thread::spawn(move || {
        for stream in listener.incoming() {
            let manager = manager.clone();
            let stream = stream.unwrap();
            thread::spawn(move || {
                let _ = handle_connection(stream, manager, Instant::now());
            });
        }
    });
    Client::at_socket(socket, "veynode-core/1")
}

fn code(error: anyhow::Error) -> ErrorCode {
    error
        .downcast_ref::<ApiError>()
        .expect("an API refusal")
        .code
}

/// A streaming call to its end, with its progress messages.
fn stream(client: &Client, method: &str, args: Value) -> (anyhow::Result<Value>, Vec<String>) {
    let mut progress = Vec::new();
    let result = client.stream(method, args).and_then(|s| {
        s.finish(|event| {
            if let Event::Event(e) = event {
                progress.extend(e["progress"].as_str().map(str::to_string));
            }
            Ok(())
        })
    });
    (result, progress)
}

fn image(name: &str, bytes: u64) -> PathBuf {
    let path = home().join(format!("{name}.raw"));
    fs::write(&path, vec![0u8; 4096]).unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(bytes)
        .unwrap();
    path
}

#[test]
fn a_linux_machine_from_a_disk_through_its_stopped_life() {
    let client = daemon();
    let disk = image("lifebase", 1 << 30);
    let share = home().join("share-life");
    fs::create_dir_all(&share).unwrap();
    let (created, progress) = stream(
        &client,
        "machine.create",
        json!({"name":"life1","os":"linux","cpus":2,"memoryMiB":2048,"diskGiB":8,
               "disk":disk,"shares":[{"hostPath":share,"tag":"m0","readOnly":true}]}),
    );
    let created = created.unwrap();
    assert!(progress.iter().any(|p| p.contains("disk")), "{progress:?}");
    assert_eq!(created["name"], "life1");
    assert_eq!(created["os"], "linux");
    assert_eq!(created["backend"], "apple-virtualization");
    assert_eq!(created["state"], "stopped");
    assert_eq!(
        (created["cpus"].as_u64(), created["memoryMiB"].as_u64()),
        (Some(2), Some(2048))
    );
    assert_eq!(created["diskGiB"], 8);
    assert_eq!(
        created["source"]["disk"],
        json!(fs::canonicalize(&disk).unwrap())
    );
    assert_eq!(created["shares"][0]["guestPath"], "/mnt/m0");
    assert_eq!(created["shares"][0]["readOnly"], true);
    assert!(created["macAddress"].is_string());
    let dir = home().join("machines/life1");
    assert_eq!(fs::metadata(dir.join("disk.raw")).unwrap().len(), 8 << 30);

    // Its id names it as well as its name does.
    let id = created["id"].as_str().unwrap();
    assert_eq!(
        client.call("machine.inspect", json!({"name":id})).unwrap()["name"],
        "life1"
    );
    let names = |client: &Client| -> Vec<String> {
        client
            .call("machine.ls", json!({}))
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["name"].as_str().unwrap().to_string())
            .filter(|n| n.starts_with("life"))
            .collect()
    };
    assert_eq!(names(&client), ["life1"]);

    client
        .call(
            "machine.update",
            json!({"name":"life1","cpus":3,"memoryMiB":4096,"diskGiB":16}),
        )
        .unwrap();
    let updated = client
        .call("machine.inspect", json!({"name":"life1"}))
        .unwrap();
    assert_eq!(
        (updated["cpus"].as_u64(), updated["diskGiB"].as_u64()),
        (Some(3), Some(16))
    );
    assert_eq!(
        code(
            client
                .call("machine.update", json!({"name":"life1","diskGiB":8}))
                .unwrap_err()
        ),
        ErrorCode::Invalid
    );
    // Apple Virtualization machines publish no ports.
    assert_eq!(
        code(
            client
                .call(
                    "machine.update",
                    json!({"name":"life1","ports":[{"host":2222,"guest":22}]})
                )
                .unwrap_err()
        ),
        ErrorCode::Invalid
    );

    let copy = client
        .call("machine.clone", json!({"name":"life1","newName":"life2"}))
        .unwrap();
    assert_eq!(copy["name"], "life2");
    assert_ne!(copy["id"], created["id"]);
    assert_ne!(copy["macAddress"], updated["macAddress"]);
    assert!(home().join("machines/life2/disk.raw").is_file());
    assert_eq!(
        code(
            client
                .call("machine.clone", json!({"name":"life1","newName":"life2"}))
                .unwrap_err()
        ),
        ErrorCode::State
    );
    let renamed = client
        .call("machine.rename", json!({"name":"life2","newName":"life3"}))
        .unwrap();
    assert_eq!(renamed["name"], "life3");
    assert_eq!(names(&client), ["life1", "life3"]);

    let endpoints = client
        .call("machine.endpoints", json!({"name":"life1"}))
        .unwrap();
    assert_eq!(
        endpoints,
        json!({"address":null,"hostAddress":null,"ports":[]})
    );
    assert_eq!(
        code(
            client
                .call("machine.stop", json!({"name":"life1"}))
                .unwrap_err()
        ),
        ErrorCode::State
    );
    let (exec, _) = stream(
        &client,
        "machine.exec",
        json!({"name":"life1","argv":["true"],"stdin":false}),
    );
    assert_eq!(code(exec.unwrap_err()), ErrorCode::State);
    let (copied, _) = stream(
        &client,
        "files.copy",
        json!({"name":"life1","direction":"in","hostPath":disk,"guestPath":"/tmp/x"}),
    );
    assert_eq!(code(copied.unwrap_err()), ErrorCode::State);

    for name in ["life1", "life3"] {
        assert_eq!(
            client
                .call("machine.rm", json!({"name":name,"force":true}))
                .unwrap()["name"],
            name
        );
        assert!(!home().join("machines").join(name).exists());
    }
    assert!(names(&client).is_empty());
}

#[test]
fn refusals_carry_the_codes_the_plugin_maps() {
    let client = daemon();
    assert_eq!(
        code(
            client
                .call("machine.inspect", json!({"name":"ghost"}))
                .unwrap_err()
        ),
        ErrorCode::NotFound
    );
    assert_eq!(
        code(
            client
                .call("machine.rm", json!({"name":"ghost","force":true}))
                .unwrap_err()
        ),
        ErrorCode::NotFound
    );
    let bad = |args: Value| code(stream(&client, "machine.create", args).0.unwrap_err());
    assert_eq!(
        bad(json!({"name":"bad name","os":"linux"})),
        ErrorCode::Invalid
    );
    assert_eq!(
        bad(json!({"name":"badmem","os":"linux","memoryMiB":64})),
        ErrorCode::Invalid
    );
    assert_eq!(
        bad(json!({"name":"baddisk","os":"linux","disk":"/no/such.raw"})),
        ErrorCode::Invalid
    );
    // No QEMU runtime is registered with this daemon.
    assert_eq!(
        bad(json!({"name":"win","os":"windows","iso":"/no/such.iso"})),
        ErrorCode::Unsupported
    );
    assert_eq!(
        bad(json!({"name":"mac","os":"macos","from":"ghcr.io/x/y:1","ports":[]})),
        ErrorCode::Invalid
    );

    let (first, _) = stream(
        &client,
        "machine.create",
        json!({"name":"twice","os":"linux","diskGiB":4}),
    );
    first.unwrap();
    assert_eq!(bad(json!({"name":"twice","os":"linux"})), ErrorCode::State);
    // Refused before the image is looked up.
    assert_eq!(
        bad(json!({"name":"twice","os":"macos","from":"ghcr.io/x/y:1"})),
        ErrorCode::State
    );
    client.call("machine.rm", json!({"name":"twice"})).unwrap();
}
