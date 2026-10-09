//! Actual localhost ICE/DTLS/SCTP, no Guest/encoder/HTTP service or auth claim.
use std::{sync::Arc, time::Duration};
use tokio::{sync::watch, time::timeout};
use webrtc::{
    data_channel::{DataChannel, DataChannelEvent},
    peer_connection::{
        PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceGatheringState,
    },
};
struct Events {
    gathered: watch::Sender<bool>,
    channel: watch::Sender<Option<Arc<dyn DataChannel>>>,
}
#[async_trait::async_trait]
impl PeerConnectionEventHandler for Events {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            self.gathered.send_replace(true);
        }
    }
    async fn on_data_channel(&self, channel: Arc<dyn DataChannel>) {
        self.channel.send_replace(Some(channel));
    }
}
async fn message(channel: &dyn DataChannel) -> Vec<u8> {
    loop {
        match channel.poll().await {
            Some(DataChannelEvent::OnMessage(v)) => return v.data.to_vec(),
            None => panic!("channel ended before receipt"),
            _ => {}
        }
    }
}
#[tokio::test]
async fn actual_dtls_sctp_bidirectional_messages_and_close_after_media_fence() {
    let (a_tx, a_rx) = watch::channel(false);
    let (b_tx, b_rx) = watch::channel(false);
    let (ac, _) = watch::channel(None);
    let (bc, mut server_channel) = watch::channel(None);
    let build = |handler| {
        PeerConnectionBuilder::new()
            .with_setting_engine(crate::control_channel::bounded_settings())
            .with_handler(handler)
            .with_runtime(webrtc::runtime::default_runtime().unwrap())
            .with_udp_addrs(vec!["127.0.0.1:0".to_string()])
    };
    let client = build(Arc::new(Events {
        gathered: a_tx,
        channel: ac,
    }))
    .build()
    .await
    .unwrap();
    let server = build(Arc::new(Events {
        gathered: b_tx,
        channel: bc,
    }))
    .build()
    .await
    .unwrap();
    let gate = server.media_egress();
    let result = timeout(Duration::from_secs(15), async {
        gate.enable(108).unwrap();
        let channel = client
            .create_data_channel("egress-test", None)
            .await
            .unwrap();
        client
            .set_local_description(client.create_offer(None).await.unwrap())
            .await
            .unwrap();
        crate::flag(a_rx).await;
        eprintln!("egress test: client gathered");
        server
            .set_remote_description(client.local_description().await.unwrap())
            .await
            .unwrap();
        server
            .set_local_description(server.create_answer(None).await.unwrap())
            .await
            .unwrap();
        crate::flag(b_rx).await;
        eprintln!("egress test: server gathered {:?}", gate.snapshot());
        client
            .set_remote_description(server.local_description().await.unwrap())
            .await
            .unwrap();
        eprintln!("egress test: waiting open {:?}", gate.snapshot());
        loop {
            match channel.poll().await {
                Some(DataChannelEvent::OnOpen) => break,
                None | Some(DataChannelEvent::OnClose) => panic!("channel ended before open"),
                _ => {}
            }
        }
        eprintln!("egress test: open {:?}", gate.snapshot());
        while server_channel.borrow().is_none() {
            server_channel.changed().await.unwrap();
        }
        let remote = server_channel.borrow().clone().unwrap();
        let before = gate.fence();
        eprintln!("egress test: fenced {:?}", before);
        assert!(!before.failed);
        // Real DTLS records carrying fragmented SCTP DATA and acknowledgements
        // continue after fencing. Twelve legal 16KiB messages each direction total
        // three 64KiB exchanges; production max-message-size remains unchanged.
        // These are synthetic test bytes, not commands.
        for i in 0..12u8 {
            let request = vec![i; crate::control_channel::CHUNK_BYTES];
            channel
                .send(bytes::BytesMut::from(request.as_slice()))
                .await
                .unwrap();
            assert_eq!(message(&*remote).await, request);
            eprintln!("egress test: request {i}");
            let receipt = vec![i + 10; crate::control_channel::CHUNK_BYTES];
            remote
                .send(bytes::BytesMut::from(receipt.as_slice()))
                .await
                .unwrap();
            assert_eq!(message(&*channel).await, receipt);
            eprintln!("egress test: reply {i}");
        }
        let after = gate.snapshot();
        assert!(after.fenced && !after.failed);
        assert_eq!(
            after.media_submitted_datagrams,
            before.media_submitted_datagrams
        );
        assert!(after.control_submitted_datagrams > before.control_submitted_datagrams);
        remote.close().await.unwrap();
        channel.close().await.unwrap();
    })
    .await;
    let close = timeout(Duration::from_secs(5), async {
        tokio::join!(client.close(), server.close())
    })
    .await
    .expect("peer cleanup deadline");
    close.0.unwrap();
    close.1.unwrap();
    assert!(!gate.snapshot().pending_media_send);
    assert!(!gate.snapshot().failed);
    result.expect("actual localhost peer fence test deadline");
}
