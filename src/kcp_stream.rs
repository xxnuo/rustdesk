use hbb_common::{
    anyhow, bail,
    bytes::{Bytes, BytesMut},
    bytes_codec::BytesCodec,
    config, log,
    tcp::{DynTcpStream, FramedStream},
    tokio::{self, net::UdpSocket, sync::mpsc, sync::oneshot},
    tokio_util, ResultType, Stream,
};
use kcp_sys::{
    endpoint::{ConnId, KcpEndpoint},
    packet_def::{KcpPacket, KcpPacketHeader},
    stream,
};
use std::{
    net::SocketAddr,
    sync::{Arc, RwLock},
};

#[cfg(feature = "gdesk")]
#[path = "gdesk_datagram.rs"]
pub mod gdesk_datagram;
#[cfg(feature = "gdesk")]
pub use gdesk_datagram::{
    DatagramKind, GDeskDatagram, GDeskDatagramCrypto, GDeskDatagramLink, GDeskSeedNegotiator,
};

#[cfg(feature = "gdesk")]
pub const GDESK_DATAGRAM_CHANNEL_CAPACITY: usize = 256;

#[cfg(feature = "gdesk")]
#[derive(Clone)]
pub struct GDeskUdpSocket {
    udp_socket: Arc<UdpSocket>,
    rx: Arc<tokio::sync::Mutex<mpsc::Receiver<Bytes>>>,
    crypto: Arc<RwLock<Option<GDeskDatagramCrypto>>>,
}

#[cfg(feature = "gdesk")]
impl GDeskUdpSocket {
    fn new(udp_socket: Arc<UdpSocket>, rx: mpsc::Receiver<Bytes>) -> Self {
        Self {
            udp_socket,
            rx: Arc::new(tokio::sync::Mutex::new(rx)),
            crypto: Arc::new(RwLock::new(None)),
        }
    }

    /// Activates the directional AEAD crypto context at most once per KCP session.
    /// Refuses any second activation so a duplicate capability message can never reset sequence numbers.
    pub fn activate_crypto_once(&self, crypto: GDeskDatagramCrypto) -> ResultType<()> {
        let mut guard = self.crypto.write().unwrap();
        if guard.is_some() {
            bail!("GDesk UDP datagram crypto is already activated for this session");
        }
        *guard = Some(crypto);
        Ok(())
    }

    #[inline]
    pub fn has_crypto(&self) -> bool {
        self.crypto.read().unwrap().is_some()
    }

    pub fn crypto(&self) -> Option<GDeskDatagramCrypto> {
        self.crypto.read().unwrap().clone()
    }

    pub async fn send_raw_datagram(&self, packet: &[u8]) -> ResultType<()> {
        if packet.len() > gdesk_datagram::GDESK_UDP_MTU {
            bail!(
                "GDesk raw UDP datagram {} exceeds {}",
                packet.len(),
                gdesk_datagram::GDESK_UDP_MTU
            );
        }
        self.udp_socket.send(packet).await?;
        Ok(())
    }

    pub async fn send_datagram(
        &self,
        kind: DatagramKind,
        display: u8,
        payload: &[u8],
    ) -> ResultType<u64> {
        let (seq, wire) = {
            let guard = self.crypto.read().unwrap();
            let crypto = guard
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("GDesk UDP datagram crypto not activated"))?;
            crypto.seal_datagram(kind, display, payload)?
        };
        self.udp_socket.send(&wire).await?;
        Ok(seq)
    }

    #[cfg(feature = "gdesk")]
    pub async fn send_video_frame(&self, display: u8, frame_bytes: &[u8]) -> ResultType<usize> {
        let packets = {
            let guard = self.crypto.read().unwrap();
            let crypto = guard
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("GDesk UDP datagram crypto not activated"))?;
            crypto.seal_video_frame(display, frame_bytes)?
        };
        let count = packets.len();
        for pkt in packets {
            self.udp_socket.send(&pkt).await?;
        }
        Ok(count)
    }

    pub async fn recv_raw(&self) -> Option<Bytes> {
        self.rx.lock().await.recv().await
    }

    /// Receives the next valid, authenticated GDesk UDP datagram from the bounded demux queue,
    /// dropping any unauthenticated, replayed, or pre-key packets without stalling the caller.
    pub async fn recv_datagram(&self) -> Option<GDeskDatagram> {
        loop {
            let raw = self.rx.lock().await.recv().await?;
            let crypto = self.crypto.read().unwrap().clone();
            let Some(crypto) = crypto else {
                continue;
            };
            if let Ok(datagram) = crypto.open_datagram(&raw) {
                return Some(datagram);
            }
        }
    }
}

pub struct KcpStream {
    endpoint: KcpEndpoint,
    conn_id: ConnId,
    stop_sender: Option<oneshot::Sender<()>>,
    #[cfg(feature = "gdesk")]
    gdesk_udp: GDeskUdpSocket,
}

const KCP_IO_ERR_LOG_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
static KCP_SEND_ERR_LOG: hbb_common::log_throttle::LogThrottle =
    hbb_common::log_throttle::LogThrottle::new(KCP_IO_ERR_LOG_INTERVAL);
static KCP_RECV_ERR_LOG: hbb_common::log_throttle::LogThrottle =
    hbb_common::log_throttle::LogThrottle::new(KCP_IO_ERR_LOG_INTERVAL);

impl KcpStream {
    // Opt in to KCP's built-in congestion window (nc=0) instead of the pure turbo profile
    // (nc=1) that has always shipped; see `get_kcp_cc_enabled` for why this is not the default.
    // Sender-side only, so no wire negotiation is needed and either peer may run either profile.
    // Requires kcp-sys from the `rustdesk-patches` branch, which wires the config factory into
    // connection setup (on older revs the factory was stored but never consulted).
    fn apply_kcp_config(endpoint: &mut KcpEndpoint) {
        if crate::get_kcp_cc_enabled() {
            endpoint.set_kcp_config_factory(Box::new(|conv| {
                let mut config = kcp_sys::ffi_safe::KcpConfig::new_turbo(conv);
                config.nc = Some(0);
                config
            }));
        }
    }

    /// How long since a valid packet was last received from the peer, or `None` once the
    /// connection is gone. Answered by the KCP endpoint's own tasks, not by the session's read
    /// loop, so it stays meaningful while that loop is busy sending a large message; and the
    /// endpoint pings an idle peer often enough that silence here means the peer, not quiet.
    pub fn peer_silent_for(&self) -> Option<std::time::Duration> {
        self.endpoint.peer_silent_for(&self.conn_id)
    }

#[cfg(feature = "gdesk")]
    pub fn gdesk_udp(&self) -> GDeskUdpSocket {
        self.gdesk_udp.clone()
    }

#[cfg(feature = "gdesk")]
    pub fn gdesk_link(&self) -> GDeskDatagramLink {
        GDeskDatagramLink::new(Some(self.gdesk_udp.clone()))
    }

    fn create_framed(stream: stream::KcpStream, local_addr: Option<SocketAddr>) -> Stream {
        Stream::Tcp(FramedStream(
            tokio_util::codec::Framed::new(DynTcpStream(Box::new(stream)), BytesCodec::new()),
            local_addr.unwrap_or(config::Config::get_any_listen_addr(true)),
            None,
            0,
        ))
    }

    pub async fn accept(
        udp_socket: Arc<UdpSocket>,
        timeout: std::time::Duration,
        init_packet: Option<BytesMut>,
    ) -> ResultType<(Self, Stream)> {
        let mut endpoint = KcpEndpoint::new();
        Self::apply_kcp_config(&mut endpoint);
        endpoint.run().await;

        let (input, output) = (
            endpoint.input_sender(),
            endpoint
                .output_receiver()
                .ok_or_else(|| anyhow::anyhow!("Failed to get output receiver"))?,
        );
        let (stop_sender, stop_receiver) = oneshot::channel();
        #[cfg(feature = "gdesk")]
        let (gdesk_tx, gdesk_rx) = mpsc::channel(GDESK_DATAGRAM_CHANNEL_CAPACITY);
        #[cfg(feature = "gdesk")]
        let gdesk_udp = GDeskUdpSocket::new(udp_socket.clone(), gdesk_rx);
        if let Some(packet) = init_packet {
            if packet.len() >= std::mem::size_of::<KcpPacketHeader>() {
                input.send(packet.into()).await?;
            }
        }
        Self::kcp_io(udp_socket.clone(), input, output, #[cfg(feature = "gdesk")] gdesk_tx, stop_receiver).await;

        let conn_id = tokio::time::timeout(timeout, endpoint.accept()).await??;
        if let Some(stream) = stream::KcpStream::new(&endpoint, conn_id) {
            Ok((
                Self {
                    endpoint,
                    conn_id,
                    stop_sender: Some(stop_sender),
                    #[cfg(feature = "gdesk")]
                    gdesk_udp,
                },
                Self::create_framed(stream, udp_socket.local_addr().ok()),
            ))
        } else {
            Err(anyhow::anyhow!("Failed to create KcpStream"))
        }
    }

    pub async fn connect(
        udp_socket: Arc<UdpSocket>,
        timeout: std::time::Duration,
    ) -> ResultType<(Self, Stream)> {
        let mut endpoint = KcpEndpoint::new();
        Self::apply_kcp_config(&mut endpoint);
        endpoint.run().await;

        let (input, output) = (
            endpoint.input_sender(),
            endpoint
                .output_receiver()
                .ok_or_else(|| anyhow::anyhow!("Failed to get output receiver"))?,
        );
        let (stop_sender, stop_receiver) = oneshot::channel();
        #[cfg(feature = "gdesk")]
        let (gdesk_tx, gdesk_rx) = mpsc::channel(GDESK_DATAGRAM_CHANNEL_CAPACITY);
        #[cfg(feature = "gdesk")]
        let gdesk_udp = GDeskUdpSocket::new(udp_socket.clone(), gdesk_rx);
        Self::kcp_io(udp_socket.clone(), input, output, #[cfg(feature = "gdesk")] gdesk_tx, stop_receiver).await;

        let conn_id = endpoint.connect(timeout, 0, 0, Bytes::new()).await?;
        if let Some(stream) = stream::KcpStream::new(&endpoint, conn_id) {
            Ok((
                Self {
                    endpoint,
                    conn_id,
                    stop_sender: Some(stop_sender),
                    #[cfg(feature = "gdesk")]
                    gdesk_udp,
                },
                Self::create_framed(stream, udp_socket.local_addr().ok()),
            ))
        } else {
            Err(anyhow::anyhow!("Failed to create KcpStream"))
        }
    }

    async fn kcp_io(
        udp_socket: Arc<UdpSocket>,
        input: mpsc::Sender<KcpPacket>,
        mut output: mpsc::Receiver<KcpPacket>,
        #[cfg(feature = "gdesk")] gdesk_tx: mpsc::Sender<Bytes>,
        mut stop_receiver: oneshot::Receiver<()>,
    ) {
        let udp = udp_socket.clone();
        tokio::spawn(async move {
            let mut buf = vec![0; 1500];
            // Socket errors are ICMP unreachable on a connected UDP socket — advisory, and
            // routine while a hole forms — so treat them as loss and let KCP's pong timeout reap
            // a link that is really dead. One throttle PER DIRECTION: the error is reported once
            // and cleared, so send-ok/recv-err alternates and a shared counter never fires.
            loop {
                tokio::select! {
                    _ = &mut stop_receiver => {
                        log::debug!("KCP io loop received stop signal");
                        break;
                    }
                    Some(data) = output.recv() => {
                        if let Err(e) = udp.send(&data.inner()).await {
                            if let Some(n) = KCP_SEND_ERR_LOG.due() {
                                log::debug!("KCP send error x{n} (treated as loss), last: {e}");
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                        }
                    }
                    result = udp.recv_from(&mut buf) => {
                        match result {
                            Ok((size, _)) => {
                                #[cfg(feature = "gdesk")]
                                if gdesk_datagram::is_gdesk_datagram_packet(&buf[..size]) {
                                    if (gdesk_datagram::GDESK_DATAGRAM_MIN_LEN..=gdesk_datagram::GDESK_UDP_MTU).contains(&size) {
                                        let _ = gdesk_tx.try_send(Bytes::copy_from_slice(&buf[..size]));
                                    }
                                    continue;
                                }
                                if size < std::mem::size_of::<KcpPacketHeader>() {
                                    continue;
                                }
                                input
                                    .send(BytesMut::from(&buf[..size]).into())
                                    .await.ok();
                            }
                            Err(e) => {
                                if let Some(n) = KCP_RECV_ERR_LOG.due() {
                                    log::debug!("KCP recv error x{n} (treated as loss), last: {e}");
                                }
                                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                            }
                        }
                    }
                    else => {
                        log::debug!("KCP endpoint input closed");
                        break;
                    }
                }
            }
        });
    }
}

impl Drop for KcpStream {
    fn drop(&mut self) {
        if let Some(sender) = self.stop_sender.take() {
            let _ = sender.send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    async fn connected_pair() -> (Arc<UdpSocket>, Arc<UdpSocket>) {
        let a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        a.connect(b.local_addr().unwrap()).await.unwrap();
        b.connect(a.local_addr().unwrap()).await.unwrap();
        (Arc::new(a), Arc::new(b))
    }

    async fn establish() -> ((KcpStream, Stream), (KcpStream, Stream)) {
        let (a, b) = connected_pair().await;
        let (accept_res, connect_res) = tokio::join!(
            KcpStream::accept(b, Duration::from_secs(5), None),
            KcpStream::connect(a, Duration::from_secs(5))
        );
        (
            connect_res.expect("connect over loopback"),
            accept_res.expect("accept over loopback"),
        )
    }

    // The full client path over real loopback sockets: handshake through the kcp_io
    // pumps, framed data both ways, then a graceful close. The endpoint guard stays
    // alive across the stream drop so the FIN can go out, and the peer's framed
    // stream must end (BrokenPipe from the kcp reader) instead of hanging.
    #[tokio::test]
    async fn test_kcp_stream_loopback_roundtrip_and_close() {
        let ((_guard_a, mut stream_a), (_guard_b, mut stream_b)) = establish().await;

        stream_a
            .send_bytes(Bytes::from_static(b"ping"))
            .await
            .unwrap();
        let got = stream_b.next_timeout(5000).await.unwrap().unwrap();
        assert_eq!(&got[..], b"ping");

        stream_b
            .send_bytes(Bytes::from_static(b"pong"))
            .await
            .unwrap();
        let got = stream_a.next_timeout(5000).await.unwrap().unwrap();
        assert_eq!(&got[..], b"pong");

        drop(stream_a);
        match stream_b.next_timeout(10_000).await {
            None | Some(Err(_)) => {}
            Some(Ok(data)) => panic!("unexpected data after close: {:?}", data),
        }
    }

    // A writer that queues many frames and closes immediately must not cost the
    // reader any of them: every frame arrives intact, in order, before end-of-stream.
    // This is the client-side pin for the kcp-sys close-tail-drain semantics, through
    // the real BytesCodec framing rustdesk sessions use.
    #[tokio::test]
    async fn test_kcp_stream_close_delivers_all_frames() {
        let ((_guard_a, mut tx), (_guard_b, mut rx)) = establish().await;

        const N: usize = 50;
        let payload = vec![7u8; 32 * 1024];
        for _ in 0..N {
            tx.send_bytes(Bytes::from(payload.clone())).await.unwrap();
        }
        drop(tx);

        let mut got = 0usize;
        loop {
            match rx.next_timeout(10_000).await {
                Some(Ok(data)) => {
                    assert_eq!(data.len(), payload.len(), "frame boundary broken");
                    assert!(data.iter().all(|&b| b == 7), "frame content corrupted");
                    got += 1;
                }
                // BrokenPipe (kcp reader end) or timeout-None both end the stream.
                None | Some(Err(_)) => break,
            }
        }
        assert_eq!(got, N, "graceful close lost frames");
    }

    #[cfg(feature = "gdesk")]
    #[tokio::test]
    async fn test_kcp_and_gdesk_datagram_demux_and_single_latch_seed_exchange() {
        use hbb_common::sodiumoxide::crypto::secretbox;
        let ((kcp_client, mut stream_client), (kcp_server, mut stream_server)) = establish().await;
        let client_link = kcp_client.gdesk_link();
        let server_link = kcp_server.gdesk_link();

        // TCP fallback link reports not ready and falls back cleanly.
        let tcp_only = GDeskDatagramLink::tcp_fallback();
        assert!(!tcp_only.is_udp_ready());
        assert!(!tcp_only
            .try_send_datagram(DatagramKind::VideoFrame, 0, b"fallback")
            .await
            .unwrap());

        // Seed generation must refuse unencrypted or unauthorized streams.
        let mut client_neg = GDeskSeedNegotiator::new(true);
        let mut server_neg = GDeskSeedNegotiator::new(false);
        assert!(client_neg.ensure_local_seed(&stream_client, true).is_err());

        // Simulate split stream keys (`Encrypt::new_split` where client and server have distinct `enc.0` send keys).
        let master = secretbox::gen_key();
        let transcript = hbb_common::tcp::KxTranscript {
            initiator_pk: &[0x11u8; 32],
            responder_pk: &[0x22u8; 32],
            advertised: 1,
            picked: 1,
        };
        stream_client
            .set_negotiated_key(master.clone(), true, &transcript)
            .unwrap();
        stream_server
            .set_negotiated_key(master, false, &transcript)
            .unwrap();

        // Still refused if session is not authorized yet.
        assert!(client_neg.ensure_local_seed(&stream_client, false).is_err());
        assert!(server_neg.ensure_local_seed(&stream_server, false).is_err());

        // Once authorized, exchange seeds over the encrypted KCP stream and latch once.
        let client_seed = client_neg.ensure_local_seed(&stream_client, true).unwrap();
        let server_seed = server_neg.ensure_local_seed(&stream_server, true).unwrap();
        stream_client
            .send_raw(client_seed.to_vec())
            .await
            .unwrap();
        stream_server
            .send_raw(server_seed.to_vec())
            .await
            .unwrap();

        let rx_server_seed = stream_client.next_timeout(5000).await.unwrap().unwrap();
        let rx_client_seed = stream_server.next_timeout(5000).await.unwrap().unwrap();

        let client_crypto = client_neg
            .complete_once(
                &stream_client,
                true,
                gdesk_datagram::GDESK_DATAGRAM_VERSION,
                &rx_server_seed,
            )
            .unwrap();
        let server_crypto = server_neg
            .complete_once(
                &stream_server,
                true,
                gdesk_datagram::GDESK_DATAGRAM_VERSION,
                &rx_client_seed,
            )
            .unwrap();

        // Second negotiation or duplicate seed activation MUST fail so sequence numbers cannot reset.
        assert!(client_neg
            .complete_once(
                &stream_client,
                true,
                gdesk_datagram::GDESK_DATAGRAM_VERSION,
                &rx_server_seed,
            )
            .is_err());
        client_link.activate_crypto_once(client_crypto.clone()).unwrap();
        server_link.activate_crypto_once(server_crypto.clone()).unwrap();
        assert!(client_link.activate_crypto_once(client_crypto).is_err());
        assert!(server_link.activate_crypto_once(server_crypto).is_err());

        // Bounded channel overflow protection: flooding > 256 datagrams must not block KCP control traffic.
        for i in 0..300u16 {
            assert!(server_link
                .try_send_datagram(DatagramKind::VideoFrame, 1, &i.to_le_bytes())
                .await
                .unwrap());
        }
        stream_server
            .send_raw(b"kcp-control-intact".to_vec())
            .await
            .unwrap();
        let ctrl = stream_client.next_timeout(5000).await.unwrap().unwrap();
        assert_eq!(&ctrl[..], b"kcp-control-intact");

        let first_dgram = tokio::time::timeout(Duration::from_secs(2), client_link.recv_datagram())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first_dgram.kind, DatagramKind::VideoFrame);
        assert_eq!(first_dgram.display, 1);
        assert_eq!(first_dgram.packet_seq, 1);
    }

    #[cfg(feature = "gdesk")]
    #[tokio::test]
    async fn test_gdesk_udp_15pct_loss_proxy_zero_kcp_storm_and_converges_lossless() {
        use hbb_common::sodiumoxide::crypto::secretbox;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        // Set up a real UDP proxy between client and server sockets that injects 15% deterministic random loss
        // on GDesk AEAD datagrams while counting KCP vs GDesk packets and enforcing `len <= 1200`.
        let client_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let server_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let proxy_for_client = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let proxy_for_server = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());

        let client_addr = client_sock.local_addr().unwrap();
        let server_addr = server_sock.local_addr().unwrap();
        client_sock
            .connect(proxy_for_client.local_addr().unwrap())
            .await
            .unwrap();
        server_sock
            .connect(proxy_for_server.local_addr().unwrap())
            .await
            .unwrap();

        let gdesk_sent = Arc::new(AtomicUsize::new(0));
        let gdesk_dropped = Arc::new(AtomicUsize::new(0));
        let kcp_packets_during_video = Arc::new(AtomicUsize::new(0));
        let max_seen_udp_len = Arc::new(AtomicUsize::new(0));
        let video_phase = Arc::new(AtomicBool::new(false));
        let stop_proxy = Arc::new(AtomicBool::new(false));

        {
            let p_client = proxy_for_client.clone();
            let p_server = proxy_for_server.clone();
            let gdesk_sent = gdesk_sent.clone();
            let gdesk_dropped = gdesk_dropped.clone();
            let kcp_packets = kcp_packets_during_video.clone();
            let max_len = max_seen_udp_len.clone();
            let video_phase = video_phase.clone();
            let stop_proxy = stop_proxy.clone();
            tokio::spawn(async move {
                let mut buf_c = vec![0u8; 2048];
                let mut buf_s = vec![0u8; 2048];
                let mut rng_state = 0x9E37_79B9_7F4A_7C15u64;
                loop {
                    if stop_proxy.load(Ordering::Relaxed) {
                        break;
                    }
                    tokio::select! {
                        Ok((n, _)) = p_client.recv_from(&mut buf_c) => {
                            max_len.fetch_max(n, Ordering::Relaxed);
                            if !gdesk_datagram::is_gdesk_datagram_packet(&buf_c[..n])
                                && video_phase.load(Ordering::Relaxed)
                            {
                                kcp_packets.fetch_add(1, Ordering::Relaxed);
                            }
                            let _ = p_server.send_to(&buf_c[..n], server_addr).await;
                        }
                        Ok((n, _)) = p_server.recv_from(&mut buf_s) => {
                            max_len.fetch_max(n, Ordering::Relaxed);
                            if gdesk_datagram::is_gdesk_datagram_packet(&buf_s[..n]) {
                                gdesk_sent.fetch_add(1, Ordering::Relaxed);
                                // Independent 15% drop probability with a reproducible PRNG seed.
                                rng_state ^= rng_state << 13;
                                rng_state ^= rng_state >> 7;
                                rng_state ^= rng_state << 17;
                                let drop_it = rng_state % 100 < 15;
                                if drop_it {
                                    gdesk_dropped.fetch_add(1, Ordering::Relaxed);
                                    continue;
                                }
                            } else if video_phase.load(Ordering::Relaxed) {
                                kcp_packets.fetch_add(1, Ordering::Relaxed);
                            }
                            let _ = p_client.send_to(&buf_s[..n], client_addr).await;
                        }
                    }
                }
            });
        }

        let (accept_res, connect_res) = tokio::join!(
            KcpStream::accept(server_sock, Duration::from_secs(5), None),
            KcpStream::connect(client_sock, Duration::from_secs(5))
        );
        let (kcp_client, mut stream_client) = connect_res.unwrap();
        let (kcp_server, mut stream_server) = accept_res.unwrap();

        // Negotiate split stream keys and exchange seeds over the authenticated stream.
        let master = secretbox::gen_key();
        let transcript = hbb_common::tcp::KxTranscript {
            initiator_pk: &[0x33u8; 32],
            responder_pk: &[0x44u8; 32],
            advertised: 1,
            picked: 1,
        };
        stream_client
            .set_negotiated_key(master.clone(), true, &transcript)
            .unwrap();
        stream_server
            .set_negotiated_key(master, false, &transcript)
            .unwrap();

        let mut client_neg = GDeskSeedNegotiator::new(true);
        let mut server_neg = GDeskSeedNegotiator::new(false);
        let c_seed = client_neg.ensure_local_seed(&stream_client, true).unwrap();
        let s_seed = server_neg.ensure_local_seed(&stream_server, true).unwrap();
        stream_client.send_raw(c_seed.to_vec()).await.unwrap();
        stream_server.send_raw(s_seed.to_vec()).await.unwrap();
        let s_seed = stream_client.next_timeout(5000).await.unwrap().unwrap();
        let c_seed = stream_server.next_timeout(5000).await.unwrap().unwrap();
        let c_crypto = client_neg
            .complete_once(
                &stream_client,
                true,
                gdesk_datagram::GDESK_DATAGRAM_VERSION,
                &s_seed,
            )
            .unwrap();
        let s_crypto = server_neg
            .complete_once(
                &stream_server,
                true,
                gdesk_datagram::GDESK_DATAGRAM_VERSION,
                &c_seed,
            )
            .unwrap();

        let client_link = kcp_client.gdesk_link();
        let server_link = kcp_server.gdesk_link();
        client_link.activate_crypto_once(c_crypto).unwrap();
        server_link.activate_crypto_once(s_crypto).unwrap();

        // Rich desktop image with solid, text and gradient tiles.
        let (width, height) = (512u16, 512u16);
        let mut target_rgb = vec![0u8; width as usize * height as usize * 3];
        for y in 0..height as usize {
            for x in 0..width as usize {
                let off = (y * width as usize + x) * 3;
                let tx = x / 64;
                let ty = y / 64;
                match (tx + ty) % 3 {
                    0 => {
                        target_rgb[off] = 40;
                        target_rgb[off + 1] = 120;
                        target_rgb[off + 2] = 200;
                    }
                    1 => {
                        let fg = ((x ^ y) & 1) == 0;
                        target_rgb[off] = if fg { 240 } else { 20 };
                        target_rgb[off + 1] = if fg { 240 } else { 25 };
                        target_rgb[off + 2] = if fg { 235 } else { 30 };
                    }
                    _ => {
                        target_rgb[off] = ((x * 5 + y * 3) & 0xFF) as u8;
                        target_rgb[off + 1] = ((x * 7 + y * 11) & 0xFF) as u8;
                        target_rgb[off + 2] = ((x * 13 + y * 17) & 0xFF) as u8;
                    }
                }
            }
        }

        let mut cfg = gdesk::GDeskConfig::default();
        cfg.udp_mtu = gdesk_datagram::GDESK_DATAGRAM_MAX_PAYLOAD;
        cfg.udp_redundancy_factor = 1;
        cfg.udp_refine_redundancy_factor = 1;
        cfg.refine_min_still_frames = 1;
        cfg.max_refine_tiles_per_frame = scrap::gdesk_codec::MAX_REFINE_TILES_PER_FRAME;
        let mut encoder = gdesk::GDeskEncoder::new(width, height, cfg).unwrap();

        let mut decoder = gdesk::GDeskDecoder::new();

        video_phase.store(true, Ordering::Relaxed);
        let convergence_started = std::time::Instant::now();
        let ts_us = gdesk::monotonic_us();
        let initial = encoder
            .encode_udp_datagrams(&target_rgb, width as usize * 3, gdesk::PixelFormat::Rgb, ts_us)
            .unwrap();
        assert!(!initial.is_empty());
        for dgram in &initial {
            assert!(server_link.try_send_video_frame(0, dgram).await.unwrap());
        }

        // Closed-loop convergence over the lossy shared UDP socket:
        // Client decodes whatever datagrams survive the ~15% drop proxy immediately (0 wait),
        // sends compact `tile_ack_bitmap` over the reliable KCP control stream, and Server
        // retransmits unconfirmed tiles / refines until `decoder.canvas_rgb() == target_rgb`.
        for round in 0..200 {
            let mut last_seq = None;
            while let Ok(Some(pkt)) =
                tokio::time::timeout(Duration::from_millis(25), client_link.recv_datagram()).await
            {
                let header = gdesk::GDeskFrameHeader::from_bytes(&pkt.payload).unwrap();
                decoder.decode_udp_datagram(&pkt.payload).unwrap();
                last_seq = Some(header.frame_seq);
            }
            if let Some(seq) = last_seq {
                let bitmap = decoder.tile_ack_bitmap(seq);
                let mut ack_bytes = Vec::with_capacity(4 + bitmap.len());
                ack_bytes.extend_from_slice(&seq.to_le_bytes());
                ack_bytes.extend_from_slice(&bitmap);
                stream_client.send_raw(ack_bytes).await.unwrap();
            }
            while let Some(Ok(ack_msg)) = stream_server.next_timeout(15).await {
                let seq = u32::from_le_bytes(ack_msg[0..4].try_into().unwrap());
                encoder.apply_tile_ack_bitmap(seq, &ack_msg[4..]).unwrap();
            }
            if decoder.canvas_rgb() == target_rgb.as_slice() && !encoder.has_udp_unconfirmed() {
                break;
            }

            if round % 25 == 0 { eprintln!("UDP convergence round={round} unconfirmed={} lossless={}", encoder.has_udp_unconfirmed(), decoder.is_lossless()); }
            let refines = encoder.poll_refinement_udp_datagrams(gdesk::monotonic_us()).unwrap();
            for dgram in &refines {
                assert!(server_link.try_send_video_frame(0, dgram).await.unwrap());
            }
        }

        stop_proxy.store(true, Ordering::Relaxed);
        let total_sent = gdesk_sent.load(Ordering::Relaxed);
        let total_dropped = gdesk_dropped.load(Ordering::Relaxed);
        let kcp_ctrl = kcp_packets_during_video.load(Ordering::Relaxed);
        let max_udp = max_seen_udp_len.load(Ordering::Relaxed);

        assert!(total_sent >= 10, "expected GDesk datagrams sent, got {total_sent}");
        assert!(total_dropped > 0, "expected ~15% dropped datagrams, got 0");
        assert!(
            max_udp <= gdesk_datagram::GDESK_UDP_MTU,
            "outer UDP packet {max_udp} exceeded 1200B MTU"
        );
        assert!(
            kcp_ctrl < total_sent * 2,
            "KCP control stream experienced unexpected packet storm ({kcp_ctrl} packets)"
        );
        assert_eq!(
            decoder.canvas_rgb().len(),
            target_rgb.len(),
            "decoder canvas not initialized"
        );
        let max_error = decoder
            .canvas_rgb()
            .iter()
            .zip(&target_rgb)
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap_or(u8::MAX);
        eprintln!("UDP sent={total_sent} dropped={total_dropped} KCP={kcp_ctrl} max_udp={max_udp} MaxError={max_error} convergence={:?} resent={}", convergence_started.elapsed(), encoder.udp.resent_tiles);
        assert_eq!(
            max_error, 0,
            "desktop failed to converge to Max Error == 0 after 15% UDP loss"
        );
    }
}
