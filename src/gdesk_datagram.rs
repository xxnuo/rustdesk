use hbb_common::{
    anyhow::anyhow,
    bail,
    bytes::Bytes,
    sodiumoxide::{
        crypto::{
            generichash,
            secretbox::{self, Key, Nonce},
        },
        randombytes,
    },
    ResultType, Stream,
};
use std::sync::{Arc, Mutex};

pub const GDESK_UDP_MTU: usize = 1200;
pub const GDESK_DATAGRAM_HEADER_LEN: usize = 14;
pub const GDESK_DATAGRAM_TAG_LEN: usize = secretbox::MACBYTES;
pub const GDESK_DATAGRAM_OVERHEAD: usize = GDESK_DATAGRAM_HEADER_LEN + GDESK_DATAGRAM_TAG_LEN;
pub const GDESK_DATAGRAM_MIN_LEN: usize = GDESK_DATAGRAM_OVERHEAD;
pub const GDESK_DATAGRAM_MAX_PAYLOAD: usize = GDESK_UDP_MTU - GDESK_DATAGRAM_OVERHEAD;

pub const GDESK_DATAGRAM_PREFIX_MAGIC: [u8; 2] = *b"GD";
// Matches offset 12..14 (`flag` and `rsv`) of the 14-byte `kcp_sys::packet_def::KcpPacketHeader`.
// Every valid KCP packet has `(flag & 0x80) == 0` and `rsv == 0x00`, so setting bit 7 in byte 12
// (`0xC7`) and a non-zero byte 13 (`0x64`) guarantees zero collision even for arbitrary random `conv`.
pub const GDESK_DATAGRAM_RSV_MAGIC: [u8; 2] = [0xC7, 0x64];

pub const GDESK_DATAGRAM_CONTEXT: &[u8] = b"gdesk-datagram-v1";
const GDESK_SEED_CONTEXT: &[u8] = b"gdesk-seed-v1";
pub const GDESK_DATAGRAM_VERSION: u32 = 1;
pub const GDESK_SEED_BYTES: usize = 32;

const DIR_CLIENT_TO_SERVER: u8 = 1;
const DIR_SERVER_TO_CLIENT: u8 = 2;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatagramKind {
    VideoFrame = 1,
    Input = 3,
}

impl DatagramKind {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::VideoFrame),
            3 => Some(Self::Input),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GDeskDatagram {
    pub kind: DatagramKind,
    pub display: u8,
    pub packet_seq: u64,
    pub payload: Vec<u8>,
}

/// Checks whether a raw UDP datagram is a GDesk stateless AEAD packet rather than a KCP packet.
#[inline]
pub fn is_gdesk_datagram_packet(buf: &[u8]) -> bool {
    buf.len() >= GDESK_DATAGRAM_HEADER_LEN
        && buf[0] == GDESK_DATAGRAM_PREFIX_MAGIC[0]
        && buf[1] == GDESK_DATAGRAM_PREFIX_MAGIC[1]
        && buf[12] == GDESK_DATAGRAM_RSV_MAGIC[0]
        && buf[13] == GDESK_DATAGRAM_RSV_MAGIC[1]
}

/// 64-bit sliding anti-replay window. State is updated ONLY after AEAD authentication succeeds.
#[derive(Debug, Clone, Default)]
pub struct ReplayWindow {
    max_seq: u64,
    bitmap: u64,
}

impl ReplayWindow {
    pub const fn new() -> Self {
        Self {
            max_seq: 0,
            bitmap: 0,
        }
    }

    pub fn max_seq(&self) -> u64 {
        self.max_seq
    }

    /// Read-only pre-check before AEAD decryption. Never mutates state.
    #[inline]
    pub fn can_accept(&self, seq: u64) -> bool {
        if seq == 0 {
            return false;
        }
        if self.max_seq == 0 || seq > self.max_seq {
            return true;
        }
        let diff = self.max_seq - seq;
        if diff >= 64 {
            return false;
        }
        ((self.bitmap >> diff) & 1) == 0
    }

    /// Commits `seq` into the window after AEAD tag verification succeeds.
    #[inline]
    pub fn record_authenticated(&mut self, seq: u64) -> bool {
        if !self.can_accept(seq) {
            return false;
        }
        if self.max_seq == 0 {
            self.max_seq = seq;
            self.bitmap = 1;
            return true;
        }
        if seq > self.max_seq {
            let shift = seq - self.max_seq;
            self.bitmap = if shift >= 64 {
                1
            } else {
                (self.bitmap << shift) | 1
            };
            self.max_seq = seq;
            return true;
        }
        let diff = self.max_seq - seq;
        self.bitmap |= 1u64 << diff;
        true
    }
}

/// Derives a fresh 32-byte local datagram seed bound to the caller's outbound stream key and CSPRNG entropy.
pub fn derive_local_datagram_seed(
    stream_send_key: &Key,
    fresh_entropy: &[u8; GDESK_SEED_BYTES],
) -> ResultType<[u8; GDESK_SEED_BYTES]> {
    if stream_send_key.0.iter().all(|&b| b == 0) {
        bail!("refusing all-zero stream key for GDesk seed derivation");
    }
    let failed = |_| anyhow!("GDesk datagram seed derivation failed");
    let mut state =
        generichash::State::new(Some(GDESK_SEED_BYTES), Some(&stream_send_key.0)).map_err(failed)?;
    state.update(GDESK_SEED_CONTEXT).map_err(failed)?;
    state.update(fresh_entropy).map_err(failed)?;
    let digest = state.finalize().map_err(failed)?;
    let mut out = [0u8; GDESK_SEED_BYTES];
    out.copy_from_slice(digest.as_ref());
    Ok(out)
}

/// Extracts the local outbound `secretbox::Key` from an encrypted `Stream::Tcp` after authorization
/// and derives a fresh 32-byte seed. Fails closed if the stream is not encrypted or not authorized.
pub fn generate_authorized_local_seed(
    stream: &Stream,
    authorized: bool,
) -> ResultType<[u8; GDESK_SEED_BYTES]> {
    if !authorized {
        bail!("refusing GDesk datagram seed generation before session authorization");
    }
    if !stream.is_secured() {
        bail!("refusing GDesk datagram seed generation on unsecured stream");
    }
    let send_key = match stream {
        Stream::Tcp(framed) => framed
            .2
            .as_ref()
            .map(|enc| &enc.0)
            .ok_or_else(|| anyhow!("stream has no active encryption state"))?,
        _ => bail!("GDesk UDP datagram requires a KCP/TCP-framed stream"),
    };
    let mut entropy = [0u8; GDESK_SEED_BYTES];
    randombytes::randombytes_into(&mut entropy);
    derive_local_datagram_seed(send_key, &entropy)
}

fn derive_directional_subkey(master: &[u8; secretbox::KEYBYTES], direction: u8) -> ResultType<Key> {
    let failed = |_| anyhow!("GDesk directional key derivation failed");
    let mut state =
        generichash::State::new(Some(secretbox::KEYBYTES), Some(master)).map_err(failed)?;
    state.update(GDESK_DATAGRAM_CONTEXT).map_err(failed)?;
    state.update(&[direction]).map_err(failed)?;
    let digest = state.finalize().map_err(failed)?;
    let mut subkey = [0u8; secretbox::KEYBYTES];
    subkey.copy_from_slice(digest.as_ref());
    Ok(Key(subkey))
}

fn build_nonce(header: &[u8; GDESK_DATAGRAM_HEADER_LEN], direction: u8) -> Nonce {
    let mut nonce = [0u8; secretbox::NONCEBYTES];
    nonce[..GDESK_DATAGRAM_HEADER_LEN].copy_from_slice(header);
    nonce[GDESK_DATAGRAM_HEADER_LEN] = direction;
    Nonce(nonce)
}

#[derive(Clone)]
pub struct GDeskDatagramCrypto {
    tx_key: Key,
    rx_key: Key,
    tx_dir: u8,
    rx_dir: u8,
    inner: Arc<Mutex<CryptoMutableState>>,
}

#[derive(Debug, Default)]
struct CryptoMutableState {
    next_tx_seq: u64,
    replay_window: ReplayWindow,
    tx_packets: u64,
    rx_packets: u64,
    rx_dropped_replay_or_auth: u64,
}

impl GDeskDatagramCrypto {
    /// Derives directional stateless AEAD subkeys from seeds exchanged over the authenticated reliable stream.
    /// Fixes the seed order to `(client_seed, server_seed)` + `"gdesk-datagram-v1"` domain.
    /// `is_client` is `true` on the client (controller) and `false` on the server (controlled).
    pub fn from_exchanged_seeds(
        local_seed: &[u8; GDESK_SEED_BYTES],
        peer_seed: &[u8; GDESK_SEED_BYTES],
        is_client: bool,
    ) -> ResultType<Self> {
        if local_seed.iter().all(|&b| b == 0) || peer_seed.iter().all(|&b| b == 0) {
            bail!("refusing all-zero datagram seed");
        }
        if local_seed == peer_seed {
            bail!("refusing identical client and server datagram seeds");
        }
        let (client_seed, server_seed) = if is_client {
            (local_seed, peer_seed)
        } else {
            (peer_seed, local_seed)
        };
        let failed = |_| anyhow!("GDesk seed combination failed");
        let mut state =
            generichash::State::new(Some(secretbox::KEYBYTES), Some(client_seed))
                .map_err(failed)?;
        state.update(GDESK_DATAGRAM_CONTEXT).map_err(failed)?;
        state.update(server_seed).map_err(failed)?;
        let digest = state.finalize().map_err(failed)?;
        let mut master = [0u8; secretbox::KEYBYTES];
        master.copy_from_slice(digest.as_ref());

        let c2s = derive_directional_subkey(&master, DIR_CLIENT_TO_SERVER)?;
        let s2c = derive_directional_subkey(&master, DIR_SERVER_TO_CLIENT)?;
        let (tx_key, rx_key, tx_dir, rx_dir) = if is_client {
            (c2s, s2c, DIR_CLIENT_TO_SERVER, DIR_SERVER_TO_CLIENT)
        } else {
            (s2c, c2s, DIR_SERVER_TO_CLIENT, DIR_CLIENT_TO_SERVER)
        };
        Ok(Self {
            tx_key,
            rx_key,
            tx_dir,
            rx_dir,
            inner: Arc::new(Mutex::new(CryptoMutableState {
                next_tx_seq: 1,
                replay_window: ReplayWindow::new(),
                tx_packets: 0,
                rx_packets: 0,
                rx_dropped_replay_or_auth: 0,
            })),
        })
    }

    /// Seals a single payload into a self-contained UDP packet of at most `GDESK_UDP_MTU` (1200) bytes.
    pub fn seal_datagram(
        &self,
        kind: DatagramKind,
        display: u8,
        payload: &[u8],
    ) -> ResultType<(u64, Vec<u8>)> {
        if payload.len() > GDESK_DATAGRAM_MAX_PAYLOAD {
            bail!(
                "GDesk datagram payload {} exceeds max {}",
                payload.len(),
                GDESK_DATAGRAM_MAX_PAYLOAD
            );
        }
        let seq = {
            let mut guard = self.inner.lock().unwrap();
            let seq = guard.next_tx_seq;
            guard.next_tx_seq = guard
                .next_tx_seq
                .checked_add(1)
                .ok_or_else(|| anyhow!("GDesk datagram sequence number exhausted"))?;
            guard.tx_packets += 1;
            seq
        };
        let mut header = [0u8; GDESK_DATAGRAM_HEADER_LEN];
        header[0..2].copy_from_slice(&GDESK_DATAGRAM_PREFIX_MAGIC);
        header[2] = kind as u8;
        header[3] = display;
        header[4..12].copy_from_slice(&seq.to_le_bytes());
        header[12..14].copy_from_slice(&GDESK_DATAGRAM_RSV_MAGIC);

        let nonce = build_nonce(&header, self.tx_dir);
        let ciphertext = secretbox::seal(payload, &nonce, &self.tx_key);
        let mut packet = Vec::with_capacity(GDESK_DATAGRAM_HEADER_LEN + ciphertext.len());
        packet.extend_from_slice(&header);
        packet.extend_from_slice(&ciphertext);
        debug_assert!(packet.len() <= GDESK_UDP_MTU);
        Ok((seq, packet))
    }

    /// Authenticates, checks anti-replay, and decrypts a single UDP datagram.
    /// The 64-bit replay window is updated ONLY after `secretbox::open` succeeds.
    pub fn open_datagram(&self, packet: &[u8]) -> ResultType<GDeskDatagram> {
        if !(GDESK_DATAGRAM_MIN_LEN..=GDESK_UDP_MTU).contains(&packet.len())
            || !is_gdesk_datagram_packet(packet) {
            bail!("invalid GDesk datagram header or length");
        }
        let mut header = [0u8; GDESK_DATAGRAM_HEADER_LEN];
        header.copy_from_slice(&packet[..GDESK_DATAGRAM_HEADER_LEN]);
        let kind = DatagramKind::from_u8(header[2])
            .ok_or_else(|| anyhow!("unknown GDesk datagram kind {}", header[2]))?;
        let display = header[3];
        let mut seq_bytes = [0u8; 8];
        seq_bytes.copy_from_slice(&header[4..12]);
        let packet_seq = u64::from_le_bytes(seq_bytes);

        {
            let mut guard = self.inner.lock().unwrap();
            if !guard.replay_window.can_accept(packet_seq) {
                guard.rx_dropped_replay_or_auth += 1;
                bail!("GDesk datagram replay or expired seq {}", packet_seq);
            }
        }

        let nonce = build_nonce(&header, self.rx_dir);
        let plaintext =
            match secretbox::open(&packet[GDESK_DATAGRAM_HEADER_LEN..], &nonce, &self.rx_key) {
                Ok(pt) => pt,
                Err(()) => {
                    self.inner.lock().unwrap().rx_dropped_replay_or_auth += 1;
                    bail!("GDesk datagram AEAD authentication failed");
                }
            };

        {
            let mut guard = self.inner.lock().unwrap();
            if !guard.replay_window.record_authenticated(packet_seq) {
                guard.rx_dropped_replay_or_auth += 1;
                bail!("GDesk datagram concurrent replay for seq {}", packet_seq);
            }
            guard.rx_packets += 1;
        }

        Ok(GDeskDatagram {
            kind,
            display,
            packet_seq,
            payload: plaintext,
        })
    }

    /// Repacks a GDesk encoded frame if needed so each outer AEAD UDP packet is strictly `<= 1200` bytes.
    #[cfg(feature = "gdesk")]
    pub fn seal_video_frame(&self, display: u8, frame_bytes: &[u8]) -> ResultType<Vec<Vec<u8>>> {
        let (_, packet) = self.seal_datagram(DatagramKind::VideoFrame, display, frame_bytes)?;
        Ok(vec![packet])
    }

    pub fn stats(&self) -> (u64, u64, u64, u64) {
        let g = self.inner.lock().unwrap();
        (
            g.tx_packets,
            g.rx_packets,
            g.rx_dropped_replay_or_auth,
            g.replay_window.max_seq(),
        )
    }
}

/// Latch-once seed negotiation state machine for a single session.
/// Guarantees that:
/// 1. Seeds are only generated/accepted when the stream is encrypted and the session is authorized.
/// 2. Negotiation happens at most once per session — duplicate `GDeskTransportCapabilities` messages
///    can never reset `next_tx_seq` or the `ReplayWindow`.
pub struct GDeskSeedNegotiator {
    is_client: bool,
    local_seed: Option<[u8; GDESK_SEED_BYTES]>,
    latched: bool,
}

impl GDeskSeedNegotiator {
    pub const fn new(is_client: bool) -> Self {
        Self {
            is_client,
            local_seed: None,
            latched: false,
        }
    }

    /// Generates (or returns the already-generated) local 32-byte seed once the stream is encrypted and authorized.
    pub fn ensure_local_seed(
        &mut self,
        stream: &Stream,
        authorized: bool,
    ) -> ResultType<[u8; GDESK_SEED_BYTES]> {
        if let Some(seed) = self.local_seed {
            if !authorized || !stream.is_secured() {
                bail!("session is not authorized or stream is not secured");
            }
            return Ok(seed);
        }
        let seed = generate_authorized_local_seed(stream, authorized)?;
        self.local_seed = Some(seed);
        Ok(seed)
    }

    /// Accepts the peer's seed and derives the session's `GDeskDatagramCrypto` exactly once.
    /// Subsequent calls return an error and never overwrite an active crypto state.
    pub fn complete_once(
        &mut self,
        stream: &Stream,
        authorized: bool,
        peer_version: u32,
        peer_seed: &[u8],
    ) -> ResultType<GDeskDatagramCrypto> {
        if self.latched {
            bail!("GDesk datagram seed negotiation already completed for this session");
        }
        if peer_version != GDESK_DATAGRAM_VERSION {
            bail!("unsupported GDesk datagram version {}", peer_version);
        }
        let peer_seed: &[u8; GDESK_SEED_BYTES] = peer_seed
            .try_into()
            .map_err(|_| anyhow!("invalid GDesk peer datagram seed length {}", peer_seed.len()))?;
        let local_seed = self.ensure_local_seed(stream, authorized)?;
        let crypto = GDeskDatagramCrypto::from_exchanged_seeds(&local_seed, peer_seed, self.is_client)?;
        self.latched = true;
        Ok(crypto)
    }

    pub fn is_latched(&self) -> bool {
        self.latched
    }
}

/// High-level transport selector that routes over the shared KCP UDP socket when available and
/// falls back cleanly to the reliable stream (TCP or unnegotiated peer).
#[derive(Clone, Default)]
pub struct GDeskDatagramLink {
    udp: Option<super::GDeskUdpSocket>,
}

impl GDeskDatagramLink {
    pub fn new(udp: Option<super::GDeskUdpSocket>) -> Self {
        Self { udp }
    }

    pub fn tcp_fallback() -> Self {
        Self { udp: None }
    }

    /// Activates the AEAD crypto context at most once. Returns `Err` if already activated or if TCP-only.
    pub fn activate_crypto_once(&self, crypto: GDeskDatagramCrypto) -> ResultType<()> {
        let Some(udp) = &self.udp else {
            bail!("cannot activate UDP datagram crypto on TCP fallback link");
        };
        udp.activate_crypto_once(crypto)
    }

    #[inline]
    pub fn is_udp_ready(&self) -> bool {
        self.udp.as_ref().is_some_and(|u| u.has_crypto())
    }

    pub async fn try_send_datagram(
        &self,
        kind: DatagramKind,
        display: u8,
        payload: &[u8],
    ) -> ResultType<bool> {
        let Some(udp) = &self.udp else {
            return Ok(false);
        };
        if !udp.has_crypto() {
            return Ok(false);
        }
        udp.send_datagram(kind, display, payload).await?;
        Ok(true)
    }

    #[cfg(feature = "gdesk")]
    pub async fn try_send_video_frame(&self, display: u8, frame_bytes: &[u8]) -> ResultType<bool> {
        let Some(udp) = &self.udp else {
            return Ok(false);
        };
        if !udp.has_crypto() {
            return Ok(false);
        }
        udp.send_video_frame(display, frame_bytes).await?;
        Ok(true)
    }

    pub async fn recv_datagram(&self) -> Option<GDeskDatagram> {
        let udp = self.udp.as_ref()?;
        udp.recv_datagram().await
    }

    pub async fn recv_raw(&self) -> Option<Bytes> {
        let udp = self.udp.as_ref()?;
        udp.recv_raw().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kcp_sys::packet_def::KcpPacket;

    #[test]
    fn kcp_headers_never_collide_with_gdesk_magic_even_when_conv_starts_with_gd() {
        let gd_conv = u32::from_le_bytes([b'G', b'D', 0x12, 0x34]);
        for conv in [
            0u32,
            1,
            gd_conv,
            u32::from_le_bytes([b'G', b'D', 0xFF, 0xFF]),
            u32::MAX,
        ] {
            for flag in 0u8..=0x7f {
                let mut pkt = KcpPacket::new(64);
                pkt.mut_header()
                    .set_conv(conv)
                    .set_src_session_id(0xC764_C764)
                    .set_dst_session_id(0x4744_4744);
                pkt.mut_header().set_syn(flag & 0x01 != 0);
                pkt.mut_header().set_ack(flag & 0x02 != 0);
                pkt.mut_header().set_fin(flag & 0x04 != 0);
                pkt.mut_header().set_data(flag & 0x08 != 0);
                pkt.mut_header().set_rst(flag & 0x10 != 0);
                pkt.mut_header().set_ping(flag & 0x20 != 0);
                pkt.mut_header().set_pong(flag & 0x40 != 0);
                let raw = pkt.inner();
                assert!(
                    !is_gdesk_datagram_packet(&raw),
                    "KCP packet with conv={conv:#x} flag={flag:#x} falsely matched GDesk magic"
                );
            }
        }
    }

    #[test]
    fn directional_subkeys_isolate_directions_and_enforce_1200_mtu() {
        let client_send_key = secretbox::gen_key();
        let server_send_key = secretbox::gen_key();
        let client_seed = derive_local_datagram_seed(&client_send_key, &[0xA1; 32]).unwrap();
        let server_seed = derive_local_datagram_seed(&server_send_key, &[0xB2; 32]).unwrap();

        let client_crypto =
            GDeskDatagramCrypto::from_exchanged_seeds(&client_seed, &server_seed, true).unwrap();
        let server_crypto =
            GDeskDatagramCrypto::from_exchanged_seeds(&server_seed, &client_seed, false).unwrap();

        let max_payload = vec![0x5Au8; GDESK_DATAGRAM_MAX_PAYLOAD];
        let (seq, wire) = server_crypto
            .seal_datagram(DatagramKind::VideoFrame, 2, &max_payload)
            .unwrap();
        assert_eq!(seq, 1);
        assert_eq!(wire.len(), GDESK_UDP_MTU);
        assert!(is_gdesk_datagram_packet(&wire));

        // Over-MTU payload must be rejected before hitting the wire.
        assert!(server_crypto
            .seal_datagram(
                DatagramKind::VideoFrame,
                0,
                &vec![0u8; GDESK_DATAGRAM_MAX_PAYLOAD + 1]
            )
            .is_err());

        // Reflection attack (feeding server's own packet back to server) must fail AEAD auth.
        assert!(server_crypto.open_datagram(&wire).is_err());

        for invalid in [wire[..GDESK_DATAGRAM_HEADER_LEN].to_vec(), {
            let mut oversized = wire.clone();
            oversized.push(0);
            oversized
        }] {
            assert!(is_gdesk_datagram_packet(&invalid));
            assert!(client_crypto.open_datagram(&invalid).is_err());
        }

        // Client opens server's packet.
        let decoded = client_crypto.open_datagram(&wire).unwrap();
        assert_eq!(decoded.kind, DatagramKind::VideoFrame);
        assert_eq!(decoded.display, 2);
        assert_eq!(decoded.packet_seq, 1);
        assert_eq!(decoded.payload, max_payload);
    }

    #[test]
    fn replay_window_updates_only_after_authentication_and_handles_out_of_order() {
        let local_seed = [0x11u8; GDESK_SEED_BYTES];
        let peer_seed = [0x22u8; GDESK_SEED_BYTES];
        let tx = GDeskDatagramCrypto::from_exchanged_seeds(&local_seed, &peer_seed, false).unwrap();
        let rx = GDeskDatagramCrypto::from_exchanged_seeds(&peer_seed, &local_seed, true).unwrap();

        let mut packets = Vec::new();
        for i in 0..80u8 {
            let (_, pkt) = tx
                .seal_datagram(DatagramKind::VideoFrame, 0, &[i; 32])
                .unwrap();
            packets.push(pkt);
        }

        // Forge a packet claiming seq = 5000: must fail authentication and MUST NOT advance replay window.
        let mut forged = packets[79].clone();
        forged[4..12].copy_from_slice(&5000u64.to_le_bytes());
        assert!(rx.open_datagram(&forged).is_err());
        assert_eq!(rx.stats().3, 0, "forged packet polluted replay window max_seq");

        // Tamper with display byte in header: must fail authentication because header is bound into Nonce.
        let mut tampered_header = packets[0].clone();
        tampered_header[3] ^= 1;
        assert!(rx.open_datagram(&tampered_header).is_err());
        assert_eq!(rx.stats().3, 0);

        // Deliver seq 10 first, then seq 5 (out of order), then seq 10 again (replay).
        assert_eq!(rx.open_datagram(&packets[9]).unwrap().packet_seq, 10);
        assert_eq!(rx.open_datagram(&packets[4]).unwrap().packet_seq, 5);
        assert!(
            rx.open_datagram(&packets[9]).is_err(),
            "duplicate seq 10 must be rejected"
        );
        assert!(
            rx.open_datagram(&packets[4]).is_err(),
            "duplicate seq 5 must be rejected"
        );

        // Advance to seq 80, making seq 16 (80 - 64) expired and seq 17 (80 - 63) still valid.
        assert_eq!(rx.open_datagram(&packets[79]).unwrap().packet_seq, 80);
        assert!(
            rx.open_datagram(&packets[15]).is_err(),
            "seq 16 is outside 64-bit window"
        );
        assert_eq!(rx.open_datagram(&packets[16]).unwrap().packet_seq, 17);
    }
}
