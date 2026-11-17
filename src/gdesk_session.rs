use crate::kcp_stream::{DatagramKind, GDeskDatagram, GDeskDatagramLink, GDeskSeedNegotiator};
use base::message_proto::{
    message, video_frame, EncodedVideoFrame, EncodedVideoFrames, GDeskTransportCapabilities,
    Message, VideoFrame,
};
use hbb_common::{bail, protobuf::Message as _, ResultType, Stream};
use std::collections::HashMap;

const VERSION: u32 = 1;
const DISABLED: u32 = u32::MAX;
const MAX_PAYLOAD: usize = 1170;

pub(crate) struct Session {
    link: Option<GDeskDatagramLink>,
    seed: GDeskSeedNegotiator,
    offered: bool,
    sent_seed: bool,
    sent_ready: bool,
    peer_ready: bool,
    disabled: bool,
    backend: HashMap<u8, String>,
}

impl Session {
    pub fn new(is_client: bool) -> Self {
        Self {
            link: None,
            seed: GDeskSeedNegotiator::new(is_client),
            offered: false,
            sent_seed: false,
            sent_ready: false,
            peer_ready: false,
            disabled: false,
            backend: HashMap::new(),
        }
    }

    pub fn set_link(&mut self, link: Option<GDeskDatagramLink>) {
        self.link = link;
    }

    fn permitted(&self, stream: &Stream, authorized_remote: bool) -> bool {
        authorized_remote && stream.is_secured() && self.link.is_some() && !self.disabled
    }

    fn capabilities(seed: Vec<u8>, ready: bool, version: u32) -> Message {
        let mut msg = Message::new();
        msg.set_gdesk_transport(GDeskTransportCapabilities {
            datagram_version: version,
            seed: seed.into(),
            ready,
            ..Default::default()
        });
        msg
    }

    pub async fn offer(&mut self, stream: &mut Stream, authorized_remote: bool) -> ResultType<()> {
        if !self.permitted(stream, authorized_remote) || self.offered {
            return Ok(());
        }
        self.offered = true;
        stream
            .send(&Self::capabilities(Vec::new(), false, VERSION))
            .await
    }

    pub async fn accept(
        &mut self,
        stream: &mut Stream,
        authorized_remote: bool,
        caps: &GDeskTransportCapabilities,
    ) -> ResultType<()> {
        if !self.permitted(stream, authorized_remote) {
            return Ok(());
        }
        if !caps.active_backend.is_empty() {
            if let Ok(display) = u8::try_from(caps.display) {
                if caps.active_backend.len() <= 64 {
                    self.backend.insert(display, caps.active_backend.clone());
                }
            }
        }
        if caps.datagram_version == 0 {
            return Ok(());
        } // C2-only capability acknowledgement.
        if caps.datagram_version != VERSION {
            self.disabled = true;
            return Ok(());
        }
        if !self.sent_seed {
            let seed = self.seed.ensure_local_seed(stream, true)?;
            stream
                .send(&Self::capabilities(seed.to_vec(), false, VERSION))
                .await?;
            self.sent_seed = true;
        }
        if !caps.seed.is_empty() && !self.seed.is_latched() {
            let crypto =
                self.seed
                    .complete_once(stream, true, caps.datagram_version, &caps.seed)?;
            if let Some(link) = &self.link {
                link.activate_crypto_once(crypto)?;
            }
        }
        if self.seed.is_latched() && !self.sent_ready {
            stream
                .send(&Self::capabilities(Vec::new(), true, VERSION))
                .await?;
            self.sent_ready = true;
        }
        if caps.ready && self.seed.is_latched() {
            self.peer_ready = true;
        }
        Ok(())
    }

    pub fn ready(&self) -> bool {
        !self.disabled
            && self.sent_ready
            && self.peer_ready
            && self.link.as_ref().is_some_and(|link| link.is_udp_ready())
    }

    pub fn receiver(&self) -> Option<GDeskDatagramLink> {
        self.ready().then(|| self.link.clone()).flatten()
    }

    pub async fn fallback(&mut self, stream: &mut Stream) {
        if self.disabled {
            return;
        }
        self.disabled = true;
        // An unsupported transport version disables only this optional lane on both peers.
        if let Err(err) = stream
            .send(&Self::capabilities(Vec::new(), false, DISABLED))
            .await
        {
            hbb_common::log::trace!("GDesk fallback notice failed: {err}");
        }
    }

    pub async fn prepare_video(
        &mut self,
        stream: &mut Stream,
        video: &VideoFrame,
    ) -> ResultType<bool> {
        if !self.ready() {
            return Ok(false);
        }
        let Ok(display) = u8::try_from(video.display) else {
            return Ok(false);
        };
        let Some(video_frame::Union::Gdesks(frames)) = &video.union else {
            return Ok(false);
        };
        if frames.frames.is_empty()
            || frames
                .frames
                .iter()
                .any(|f| !f.gdesk_datagram || f.data.len() > MAX_PAYLOAD)
        {
            return Ok(false);
        }
        for frame in &frames.frames {
            let backend = &frame.gdesk_active_backend;
            if backend.is_empty() || self.backend.get(&display) == Some(backend) {
                continue;
            }
            if backend.len() > 64 {
                return Ok(false);
            }
            let mut msg = Message::new();
            msg.set_gdesk_transport(GDeskTransportCapabilities {
                active_backend: backend.clone(),
                display: display as u32,
                ..Default::default()
            });
            stream.send(&msg).await?;
            self.backend.insert(display, backend.clone());
        }
        Ok(true)
    }

    pub async fn send_video(&mut self, msg: &Message) -> ResultType<bool> {
        if !self.ready() {
            return Ok(false);
        }
        let Some(message::Union::VideoFrame(video)) = &msg.union else {
            return Ok(false);
        };
        let Ok(display) = u8::try_from(video.display) else {
            return Ok(false);
        };
        let Some(video_frame::Union::Gdesks(frames)) = &video.union else {
            return Ok(false);
        };
        if frames.frames.len() != 1
            || !frames.frames[0].gdesk_datagram
            || frames.frames[0].data.len() > MAX_PAYLOAD
        {
            return Ok(false);
        }
        match &self.link {
            Some(link) => {
                link.try_send_video_frame(display, &frames.frames[0].data)
                    .await
            }
            None => Ok(false),
        }
    }

    pub fn received_video(&self, datagram: GDeskDatagram) -> ResultType<Vec<u8>> {
        if !self.ready()
            || datagram.kind != DatagramKind::VideoFrame
            || datagram.payload.len() > MAX_PAYLOAD
        {
            bail!("unexpected GDesk UDP business packet");
        }
        let mut frames = EncodedVideoFrames::new();
        frames.frames.push(EncodedVideoFrame {
            data: datagram.payload.into(),
            gdesk_datagram: true,
            gdesk_active_backend: self
                .backend
                .get(&datagram.display)
                .cloned()
                .unwrap_or_default(),
            ..Default::default()
        });
        let mut video = VideoFrame::new();
        video.display = datagram.display as i32;
        video.set_gdesks(frames);
        let mut msg = Message::new();
        msg.set_video_frame(video);
        Ok(msg.write_to_bytes()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kcp_stream::KcpStream;
    use hbb_common::{
        sodiumoxide::crypto::secretbox,
        tokio::{
            self,
            net::UdpSocket,
            time::{timeout, Duration},
        },
    };
    use std::sync::Arc;

    async fn accept_message(session: &mut Session, stream: &mut Stream) {
        let bytes = timeout(Duration::from_secs(5), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let msg = Message::parse_from_bytes(&bytes).unwrap();
        let Some(message::Union::GdeskTransport(caps)) = msg.union else {
            panic!("expected reliable capabilities");
        };
        session.accept(stream, true, &caps).await.unwrap();
    }

    #[tokio::test]
    async fn gdesk_session_encrypted_kcp_ready_barrier_metadata_and_fallback() {
        let client_udp = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let server_udp = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        client_udp
            .connect(server_udp.local_addr().unwrap())
            .await
            .unwrap();
        server_udp
            .connect(client_udp.local_addr().unwrap())
            .await
            .unwrap();
        let (client, server) = tokio::join!(
            KcpStream::connect(client_udp, Duration::from_secs(5)),
            KcpStream::accept(server_udp, Duration::from_secs(5), None)
        );
        let (client_guard, mut client_stream) = client.unwrap();
        let (server_guard, mut server_stream) = server.unwrap();
        let mut client = Session::new(true);
        let mut server = Session::new(false);
        client.set_link(Some(client_guard.gdesk_link()));
        server.set_link(Some(server_guard.gdesk_link()));
        server.offer(&mut server_stream, false).await.unwrap();
        assert!(!server.offered);
        server.offer(&mut server_stream, true).await.unwrap();
        assert!(!server.offered); // unsecured is kept on the original reliable path.
        let transcript = hbb_common::tcp::KxTranscript {
            initiator_pk: &[11; 32],
            responder_pk: &[22; 32],
            advertised: 1,
            picked: 1,
        };
        let master = secretbox::gen_key();
        client_stream
            .set_negotiated_key(master.clone(), true, &transcript)
            .unwrap();
        server_stream
            .set_negotiated_key(master, false, &transcript)
            .unwrap();
        server.offer(&mut server_stream, true).await.unwrap();
        accept_message(&mut client, &mut client_stream).await;
        assert!(!client.ready());
        accept_message(&mut server, &mut server_stream).await;
        assert!(!server.ready());
        accept_message(&mut client, &mut client_stream).await;
        assert!(!client.ready());
        accept_message(&mut client, &mut client_stream).await;
        assert!(client.ready());
        accept_message(&mut server, &mut server_stream).await;
        assert!(server.ready());
        // A repeated Ready is idempotent and cannot reinstall keys or reset packet numbers.
        server
            .accept(
                &mut server_stream,
                true,
                &GDeskTransportCapabilities {
                    datagram_version: VERSION,
                    ready: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let mut frames = EncodedVideoFrames::new();
        frames.frames.push(EncodedVideoFrame {
            data: vec![1; 1170].into(),
            gdesk_datagram: true,
            gdesk_active_backend: "GpuVulkan".into(),
            ..Default::default()
        });
        let mut video = VideoFrame::new();
        video.display = 2;
        video.set_gdesks(frames);
        assert!(server
            .prepare_video(&mut server_stream, &video)
            .await
            .unwrap());
        accept_message(&mut client, &mut client_stream).await;
        let mut msg = Message::new();
        msg.set_video_frame(video.clone());
        assert!(server.send_video(&msg).await.unwrap());
        let packet = timeout(
            Duration::from_secs(5),
            client.receiver().unwrap().recv_datagram(),
        )
        .await
        .unwrap()
        .unwrap();
        let restored = Message::parse_from_bytes(&client.received_video(packet).unwrap()).unwrap();
        assert_eq!(
            restored.video_frame().gdesks().frames[0].gdesk_active_backend,
            "GpuVulkan"
        );
        assert_eq!(
            restored.video_frame().gdesks().frames[0].data.as_ref(),
            &[1; 1170]
        );
        video.display = 256;
        assert!(!server
            .prepare_video(&mut server_stream, &video)
            .await
            .unwrap());
        server.fallback(&mut server_stream).await;
        accept_message(&mut client, &mut client_stream).await;
        assert!(!server.ready() && !client.ready());
        let mut close = Message::new();
        let mut misc = base::message_proto::Misc::new();
        misc.set_close_reason("done".into());
        close.set_misc(misc);
        server_stream.send(&close).await.unwrap();
        let wire = client_stream.next_timeout(5000).await.unwrap().unwrap();
        assert_eq!(Message::parse_from_bytes(&wire).unwrap(), close);
    }
}
