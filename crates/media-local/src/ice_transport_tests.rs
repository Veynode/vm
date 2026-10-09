//! Actual local coturn + ICE/DTLS/SCTP. No VM, Internet, Cloudflare or video claim.
//! Run explicitly with VEYNODE_TEST_ICE_CONFIG pointing to a fresh private fixture.
use super::*;
use bytes::BytesMut;
use rtc::{
    peer_connection::transport::RTCIceCandidateType,
    statistics::{report::RTCStatsReportEntry, StatsSelector},
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, watch};
use webrtc::{
    data_channel::{DataChannel, DataChannelEvent},
    peer_connection::{
        PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceGatheringState,
    },
};
struct Events {
    gathered: watch::Sender<bool>,
    incoming: mpsc::Sender<Arc<dyn DataChannel>>,
}
#[async_trait::async_trait]
impl PeerConnectionEventHandler for Events {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            self.gathered.send_replace(true);
        }
    }
    async fn on_data_channel(&self, channel: Arc<dyn DataChannel>) {
        let _ = self.incoming.try_send(channel);
    }
}
async fn peer(
    config: &IceConfig,
) -> Result<(
    Arc<dyn PeerConnection>,
    watch::Receiver<bool>,
    mpsc::Receiver<Arc<dyn DataChannel>>,
)> {
    let (tx, rx) = watch::channel(false);
    let (incoming, channels) = mpsc::channel(1);
    let peer: Arc<dyn PeerConnection> = Arc::new(
        PeerConnectionBuilder::new()
            .with_configuration(configuration(Some(config), crate::auth::unix_now()?)?)
            .with_setting_engine(crate::control_channel::bounded_settings())
            .with_handler(Arc::new(Events {
                gathered: tx,
                incoming,
            }))
            .with_runtime(webrtc::runtime::default_runtime().context("runtime unavailable")?)
            .with_udp_addrs(vec!["127.0.0.1:0".to_string()])
            .build()
            .await?,
    );
    Ok((peer, rx, channels))
}
async fn open(channel: &Arc<dyn DataChannel>) -> Result<()> {
    loop {
        match channel.poll().await {
            Some(DataChannelEvent::OnOpen) => return Ok(()),
            Some(_) => {}
            None => anyhow::bail!("channel closed"),
        }
    }
}
async fn verify_relay(peer: &Arc<dyn PeerConnection>) -> Result<()> {
    let stats = peer.get_stats(Instant::now(), StatsSelector::None).await;
    let transport = stats.transport().context("transport stats absent")?;
    let RTCStatsReportEntry::IceCandidatePair(pair) = stats
        .get(&transport.selected_candidate_pair_id)
        .context("selected pair absent")?
    else {
        anyhow::bail!("not selected pair")
    };
    // rtc 0.21's selected pair IDs are raw ICE IDs; local report IDs are
    // prefixed in peer_connection/internal.rs::record_local_candidate_stats.
    // It currently omits remote-candidate records for SDP candidates. Check
    // the actual selected LOCAL candidate on BOTH peers instead of inventing
    // a remote stats record or treating a configured TURN URL as proof.
    let RTCStatsReportEntry::LocalCandidate(local) = stats
        .get(&format!("RTCLocalIceCandidate_{}", pair.local_candidate_id))
        .context("selected local stats absent")?
    else {
        anyhow::bail!("not local candidate")
    };
    ensure!(
        local.candidate_type == RTCIceCandidateType::Relay,
        "selected local candidate is not relay"
    );
    ensure!(
        transport.bytes_sent > 0 && transport.bytes_received > 0,
        "no transport traffic"
    );
    Ok(())
}
fn fixture() -> IceConfig {
    read_private(
        std::path::Path::new(
            &std::env::var("VEYNODE_TEST_ICE_CONFIG")
                .expect("explicit private local TURN fixture required"),
        ),
        crate::auth::unix_now().unwrap(),
    )
    .unwrap()
}
#[tokio::test]
#[ignore = "requires explicitly started loopback coturn and fresh private config"]
async fn actual_local_turn_forced_relay_dtls_sctp_payload_and_selected_stats() {
    let config = fixture();
    let (a, a_gather, _) = peer(&config).await.unwrap();
    let (b, b_gather, mut channels) = peer(&config).await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        let channel = a.create_data_channel("local-relay-test", None).await?;
        a.set_local_description(a.create_offer(None).await?).await?;
        crate::flag(a_gather).await;
        let offer = a.local_description().await.context("offer absent")?;
        ensure!(
            offer
                .sdp
                .lines()
                .filter(|l| l.starts_with("a=candidate:"))
                .all(|l| l.contains(" typ relay")),
            "force-relay leaked other candidates"
        );
        b.set_remote_description(offer).await?;
        b.set_local_description(b.create_answer(None).await?)
            .await?;
        crate::flag(b_gather).await;
        a.set_remote_description(b.local_description().await.context("answer absent")?)
            .await?;
        open(&channel).await?;
        let incoming = channels.recv().await.context("incoming channel absent")?;
        open(&incoming).await?;
        let payload = vec![0xa5; 4096];
        channel.send(BytesMut::from(payload.as_slice())).await?;
        loop {
            if let Some(DataChannelEvent::OnMessage(m)) = incoming.poll().await {
                ensure!(
                    !m.is_string && m.data.as_ref() == payload,
                    "relayed payload changed"
                );
                break;
            }
        }
        incoming.send(BytesMut::from(&b"relay-ack"[..])).await?;
        loop {
            if let Some(DataChannelEvent::OnMessage(m)) = channel.poll().await {
                ensure!(&m.data[..] == b"relay-ack", "reverse relay changed");
                break;
            }
        }
        verify_relay(&a).await?;
        verify_relay(&b).await?;
        Ok::<_, anyhow::Error>(())
    })
    .await;
    for p in [&a, &b] {
        tokio::time::timeout(Duration::from_secs(2), p.close())
            .await
            .unwrap()
            .unwrap();
    }
    result.expect("bounded local TURN connection").unwrap();
}
#[tokio::test]
#[ignore = "requires explicitly started loopback coturn and fresh private config"]
async fn actual_local_turn_wrong_credential_has_no_direct_fallback() {
    let mut config = fixture();
    config.ice_servers[0].credential = "wrong-credential-fixture".into();
    let (p, gather, _) = peer(&config).await.unwrap();
    let result = async {
        p.create_data_channel("must-not-open", None).await?;
        p.set_local_description(p.create_offer(None).await?).await?;
        let _ = tokio::time::timeout(Duration::from_secs(6), crate::flag(gather)).await;
        let sdp = p.local_description().await.context("offer absent")?.sdp;
        ensure!(
            !sdp.lines().any(|l| l.starts_with("a=candidate:")),
            "bad TURN credentials gathered fallback candidates"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    tokio::time::timeout(Duration::from_secs(2), p.close())
        .await
        .unwrap()
        .unwrap();
    result.unwrap();
}

#[tokio::test]
#[ignore = "requires expired REST credential fixture signed by the actual local TURN issuer"]
async fn actual_local_turn_expired_issuer_credential_has_no_direct_fallback() {
    let path =
        std::env::var("VEYNODE_TEST_EXPIRED_ICE_CONFIG").expect("explicit expired-issuer fixture");
    // Admission metadata is fresh on purpose. Only the TURN issuer can establish
    // actual password expiry; arbitrary password metadata is not cryptographic proof.
    let config = read_private(
        std::path::Path::new(&path),
        crate::auth::unix_now().unwrap(),
    )
    .unwrap();
    let (p, gather, _) = peer(&config).await.unwrap();
    let result = async {
        p.create_data_channel("expired-must-not-open", None).await?;
        p.set_local_description(p.create_offer(None).await?).await?;
        let _ = tokio::time::timeout(Duration::from_secs(6), crate::flag(gather)).await;
        let sdp = p.local_description().await.context("offer absent")?.sdp;
        ensure!(
            !sdp.lines().any(|l| l.starts_with("a=candidate:")),
            "expired TURN credential gathered fallback candidates"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    tokio::time::timeout(Duration::from_secs(2), p.close())
        .await
        .unwrap()
        .unwrap();
    result.unwrap();
}

#[tokio::test]
#[ignore = "requires a new exclusive coturn fixture with user quota4 and verbose private log"]
async fn actual_local_turn_close_releases_each_allocation_before_next_connection() {
    let directory = std::path::PathBuf::from(
        std::env::var("VEYNODE_TEST_TURN_DIR").expect("exclusive fixture directory"),
    );
    let config = read_private(
        &directory.join("ice.json"),
        crate::auth::unix_now().unwrap(),
    )
    .unwrap();
    let log = directory.join("turn.log");
    for iteration in 0..6 {
        let before = std::fs::read_to_string(&log)
            .unwrap()
            .matches("lifetime=0")
            .count();
        let deleted = std::fs::read_to_string(&log)
            .unwrap()
            .matches("allocation delete:")
            .count();
        let (p, gather, _) = peer(&config).await.unwrap();
        let setup = async {
            p.create_data_channel("allocation-cleanup-only", None)
                .await?;
            p.set_local_description(p.create_offer(None).await?).await?;
            tokio::time::timeout(Duration::from_secs(4), crate::flag(gather)).await?;
            let sdp = p.local_description().await.context("offer absent")?.sdp;
            ensure!(
                sdp.lines()
                    .any(|l| l.starts_with("a=candidate:") && l.contains(" typ relay")),
                "allocation absent; iteration {iteration}"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let closed = tokio::time::timeout(Duration::from_secs(2), p.close()).await;
        setup.unwrap();
        closed.unwrap().unwrap();
        let acknowledged = tokio::time::timeout(Duration::from_millis(500), async {
            loop {
                if std::fs::read_to_string(&log)
                    .unwrap()
                    .matches("lifetime=0")
                    .count()
                    > before
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(
            acknowledged.is_ok(),
            "peer.close returned success without server allocation lifetime0; iteration {iteration}"
        );
        // coturn4.18 ACKs zero immediately but frees quota on its ~1s timer.
        // Observe actual deletion, rather than sleeping or increasing quota.
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if std::fs::read_to_string(&log)
                    .unwrap()
                    .matches("allocation delete:")
                    .count()
                    > deleted
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("server did not delete allocation within2s after authenticated lifetime0");
    }
}

#[tokio::test]
#[ignore = "requires a fresh exclusive coturn fixture; UDP proxy withholds authentic Refresh0 ACK"]
async fn actual_local_turn_forged_or_missing_release_ack_is_bounded_error() {
    use rtc::stun::message::{Message, CLASS_SUCCESS_RESPONSE, METHOD_REFRESH};
    let mut config = fixture();
    let server: std::net::SocketAddr = config.ice_servers[0].urls[0]
        .strip_prefix("turn:")
        .unwrap()
        .strip_suffix("?transport=udp")
        .unwrap()
        .parse()
        .unwrap();
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let address = socket.local_addr().unwrap();
    config.ice_servers[0].urls[0] = format!("turn:{address}?transport=udp");
    let forged = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = forged.clone();
    let proxy = tokio::spawn(async move {
        let mut client = None;
        let mut bytes = [0u8; 4096];
        loop {
            let (n, from) = socket.recv_from(&mut bytes).await.unwrap();
            if from == server {
                let Some(client) = client else { continue };
                let mut message = Message::new();
                message.raw = bytes[..n].to_vec();
                if message.decode().is_ok()
                    && message.typ.method == METHOD_REFRESH
                    && message.typ.class == CLASS_SUCCESS_RESPONSE
                {
                    // Matching transaction/method/lifetime but invalid HMAC must
                    // not turn an unauthenticated network response into success.
                    let mut bad = bytes[..n].to_vec();
                    let mut offset = 20;
                    let mut changed = false;
                    while offset + 4 <= bad.len() {
                        let typ = u16::from_be_bytes([bad[offset], bad[offset + 1]]);
                        let len = u16::from_be_bytes([bad[offset + 2], bad[offset + 3]]) as usize;
                        if offset + 4 + len > bad.len() {
                            break;
                        }
                        if typ == 8 && len == 20 {
                            bad[offset + 4] ^= 1;
                            changed = true;
                            break;
                        }
                        offset += 4 + ((len + 3) & !3);
                    }
                    assert!(changed, "real coturn success lacked message integrity");
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    socket.send_to(&bad, client).await.unwrap();
                    continue; // Never forward the valid acknowledgement.
                }
                socket.send_to(&bytes[..n], client).await.unwrap();
            } else {
                client = Some(from);
                socket.send_to(&bytes[..n], server).await.unwrap();
            }
        }
    });
    let (p, gather, _) = peer(&config).await.unwrap();
    let setup = async {
        p.create_data_channel("negative-cleanup-only", None).await?;
        p.set_local_description(p.create_offer(None).await?).await?;
        tokio::time::timeout(Duration::from_secs(4), crate::flag(gather)).await?;
        ensure!(
            p.local_description()
                .await
                .context("offer absent")?
                .sdp
                .contains(" typ relay"),
            "proxy allocation unavailable"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let started = Instant::now();
    let closed = tokio::time::timeout(Duration::from_secs(2), p.close()).await;
    proxy.abort();
    let _ = proxy.await;
    setup.unwrap();
    assert!(
        closed.expect("close exceeded bounded timeout").is_err(),
        "forged or missing deallocation ACK was treated as success"
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(
        forged.load(std::sync::atomic::Ordering::SeqCst) > 0,
        "proxy did not inject the forged matching response"
    );
    assert!(
        p.close().await.is_err(),
        "cleanup failure must stay unconfirmed on retry"
    );
}
