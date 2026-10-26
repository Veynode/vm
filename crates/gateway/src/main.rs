//! `vm-gateway`: the transport-only relay between browsers and VeynodeVM
//! hosts for remote desktop. It terminates TLS (wss) and QUIC (WebTransport)
//! for browsers, accepts pinned host tunnels, and routes opaque end-to-end
//! encrypted messages between them. It holds no accounts and cannot read
//! or forge desktop traffic; see docs/remote-desktop.md.
mod acme;
mod certs;
mod routes;
mod state;
mod tunnel;
mod web;
mod wss;
mod wt;

#[cfg(test)]
mod loopback_test;

use anyhow::{bail, Context, Result};
use certs::CertStore;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use vm_remote_proto::config::{Config, Tls};

/// The tunnel certificate is self-signed around a kept key; hosts pin the
/// key, so the certificate's own lifetime only needs to be long.
const TUNNEL_CERT_DAYS: i64 = 3650;
const TUNNEL_NAME: &str = "veynode-tunnel";

const USAGE: &str = "usage: vm-gateway --config <gateway.toml>
       vm-gateway pin --config <gateway.toml>   print the tunnel key pin for hosts";

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut command = None;
    let mut config = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--config" => config = Some(PathBuf::from(args.next().context(USAGE)?)),
            "pin" if command.is_none() => command = Some("pin"),
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ => bail!("unexpected argument {a}\n{USAGE}"),
        }
    }
    let path = config.context(USAGE)?;
    let config = Config::load(&path)?;
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    match command {
        Some("pin") => {
            let tunnel = config.tunnel.as_ref().context("no [tunnel] in config")?;
            let key = vm_remote_proto::keys::persistent_key(&tunnel.key)?;
            println!("{}", vm_remote_proto::keys::key_pin(&key));
            Ok(())
        }
        _ => tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(serve(config)),
    }
}

async fn serve(config: Config) -> Result<()> {
    let hosts = Arc::new(tunnel::Hosts::default());
    let routes = Arc::new(routes::Routes::default());

    if let Some(t) = &config.tunnel {
        let key = vm_remote_proto::keys::persistent_key(&t.key)?;
        let tunnel_pin = vm_remote_proto::keys::key_pin(&key);
        let store = CertStore::new(certs::with_key(
            &key,
            &[TUNNEL_NAME.into()],
            TUNNEL_CERT_DAYS,
        )?);
        let endpoint = tunnel::endpoint(t.listen, store)
            .with_context(|| format!("tunnel listen {}", t.listen))?;
        eprintln!("vm-gateway: tunnel on {} (pin {tunnel_pin})", t.listen);
        tokio::spawn(tunnel::serve(
            endpoint,
            t.hosts.clone(),
            hosts.clone(),
            routes.clone(),
        ));
    }

    let public = match &config.tls {
        None | Some(Tls::External) => None,
        Some(Tls::Files { cert, key }) => Some(CertStore::new(certs::from_files(cert, key)?)),
        Some(Tls::SelfSigned { names }) => Some(CertStore::new(certs::self_signed(names)?)),
        Some(Tls::Acme { domains, .. }) => {
            // Until the first certificate arrives, serve a throwaway one so
            // the listeners (and the HTTP-01 answers) can start.
            Some(CertStore::new(certs::self_signed(domains)?))
        }
    };
    let state = state::State::new(config.clone(), public.clone(), hosts, routes);

    match (&config.tls, &public) {
        (
            Some(Tls::Acme {
                domains,
                email,
                state_dir,
                staging,
            }),
            Some(store),
        ) => {
            let acme = acme::Acme {
                domains: domains.clone(),
                email: email.clone(),
                dir: state_dir.clone(),
                staging: *staging,
                answers: state.acme.clone(),
            };
            if let Some(stored) = acme.stored()? {
                store.replace(stored);
            }
            tokio::spawn(acme.maintain(store.clone()));
        }
        (Some(Tls::SelfSigned { names }), Some(store)) => {
            tokio::spawn(rotate_self_signed(names.clone(), store.clone()));
        }
        (Some(Tls::Files { cert, key }), Some(store)) => {
            tokio::spawn(reload_on_hangup(cert.clone(), key.clone(), store.clone()));
        }
        _ => {}
    }

    if let Some(listen) = &config.listen {
        if let Some(addr) = listen.http {
            let tcp = tokio::net::TcpListener::bind(addr)
                .await
                .with_context(|| format!("listen.http {addr}"))?;
            let app = web::http_router(state.clone());
            tokio::spawn(async move { axum::serve(tcp, app).await });
        }
        let tcp = tokio::net::TcpListener::bind(listen.tcp)
            .await
            .with_context(|| format!("listen.tcp {}", listen.tcp))?;
        let app = web::router(state.clone()).into_make_service_with_connect_info::<web::Peer>();
        match &public {
            None => {
                eprintln!(
                    "vm-gateway: http on {} (TLS terminated in front)",
                    listen.tcp
                );
                tokio::spawn(async move { axum::serve(tcp, app).await });
            }
            Some(store) => {
                eprintln!("vm-gateway: https on {}", listen.tcp);
                let tls = web::TlsListener::new(tcp, store.clone())?;
                tokio::spawn(async move { axum::serve(tls, app).await });
            }
        }
        if let (Some(addr), Some(store)) = (listen.udp, &public) {
            let endpoint =
                wt::endpoint(addr, store.clone()).with_context(|| format!("listen.udp {addr}"))?;
            eprintln!("vm-gateway: webtransport on {addr}");
            tokio::spawn(wt::serve(endpoint, state.clone()));
        }
    }

    shutdown().await;
    Ok(())
}

async fn rotate_self_signed(names: Vec<String>, store: Arc<CertStore>) {
    let every = certs::SELF_SIGNED_LIFETIME - certs::SELF_SIGNED_ROTATE_BEFORE;
    loop {
        tokio::time::sleep(every).await;
        match certs::self_signed(&names) {
            Ok(key) => {
                store.replace(key);
                eprintln!(
                    "vm-gateway: self-signed certificate rotated, sha256 {}",
                    certs::pin(&store.leaf_sha256())
                );
            }
            Err(e) => eprintln!("vm-gateway: rotate certificate: {e:#}"),
        }
    }
}

async fn reload_on_hangup(cert: PathBuf, key: PathBuf, store: Arc<CertStore>) {
    use tokio::signal::unix::{signal, SignalKind};
    let Ok(mut hup) = signal(SignalKind::hangup()) else {
        return;
    };
    while hup.recv().await.is_some() {
        match certs::from_files(&cert, &key) {
            Ok(k) => {
                store.replace(k);
                eprintln!("vm-gateway: certificate reloaded");
            }
            Err(e) => eprintln!("vm-gateway: reload certificate: {e:#}"),
        }
    }
}

async fn shutdown() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(_) => return std::future::pending().await,
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    // Give QUIC peers a moment to see the close.
    tokio::time::sleep(Duration::from_millis(100)).await;
}
