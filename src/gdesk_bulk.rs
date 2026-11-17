use base::{fs, message_proto::*};
use gdesk::sched::{Packet, Priority, SendScheduler, P2_MAX_CHUNK};
use hbb_common::{bail, protobuf::Message as _, ResultType, Stream};
use std::{collections::{HashMap, VecDeque}, time::{Duration, Instant}};

pub const BULK_VERSION: u32 = 2;
const BULK_WINDOW: u64 = 4;
const CHUNK_BYTES: usize = P2_MAX_CHUNK;
const MAX_MESSAGE: usize = 64 * 1024 * 1024;
const MAX_BUFFERED: usize = 128 * 1024 * 1024;
const MAX_TRANSFERS: usize = 4;
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(30);

struct Outgoing {
    transfer: u64,
    data: Vec<u8>,
    offset: usize,
}

struct Assembly {
    total: usize,
    next: u32,
    started: Instant,
    data: Vec<u8>,
}

#[derive(Default)]
pub(crate) struct Transport {
    negotiated: bool,
    active: bool,
    scheduler: SendScheduler,
    outgoing: [VecDeque<Outgoing>; 3],
    queued_bytes: usize,
    next_transfer: u64,
    sent_credit: u64,
    acked_credit: u64,
    incoming: HashMap<u64, Assembly>,
    reserved_bytes: usize,
    failure: Option<String>,
}

impl Transport {
    pub fn negotiate(&mut self, version: u32) {
        if version == BULK_VERSION { self.negotiated = true; }
    }

    pub fn set_active(&mut self, active: bool) {
        self.active = self.negotiated && active;
    }

    pub fn is_active(&self) -> bool { self.active }
    pub fn is_empty(&self) -> bool { self.scheduler.is_empty() }
    pub fn has_video(&self) -> bool { self.scheduler.has_framed_video() }

    pub fn check(&self, now: Instant) -> ResultType<()> {
        if let Some(err) = &self.failure { bail!("GDesk send queue: {err}"); }
        if self.incoming.values().any(|a| now.duration_since(a.started) > TRANSFER_TIMEOUT) {
            bail!("GDesk bulk reassembly timed out");
        }
        Ok(())
    }

    pub fn next_ready_at_with_bulk(&self, allow_bulk: bool) -> Option<Instant> {
        self.scheduler.next_ready_at_with_bulk(Instant::now(), allow_bulk && self.bulk_ready())
    }

    pub fn enqueue(&mut self, msg: &Message, now: Instant) -> ResultType<()> {
        let result = self.enqueue_inner(msg, now);
        if let Err(err) = &result { self.failure = Some(err.to_string()); }
        result
    }

    fn enqueue_inner(&mut self, msg: &Message, now: Instant) -> ResultType<()> {
        let size = msg.compute_size() as usize;
        if size > MAX_MESSAGE || self.queued_bytes + size > MAX_BUFFERED {
            bail!("GDesk outgoing message/queue exceeds memory limit");
        }
        if size > CHUNK_BYTES && !bulk_business(msg) {
            bail!("GDesk cannot fragment this message type");
        }
        self.next_transfer = self.next_transfer.checked_add(1)
            .ok_or_else(|| hbb_common::anyhow::anyhow!("GDesk transfer id exhausted"))?;
        let lane = lane(priority(msg));
        let data = msg.write_to_bytes()?;
        self.queued_bytes += data.len();
        self.outgoing[lane].push_back(Outgoing { transfer: self.next_transfer, data, offset: 0 });
        if self.outgoing[lane].len() == 1 { self.prepare(lane, now)?; }
        Ok(())
    }

    fn prepare(&mut self, lane: usize, now: Instant) -> ResultType<()> {
        let Some(entry) = self.outgoing[lane].front_mut() else { return Ok(()); };
        let end = (entry.offset + CHUNK_BYTES).min(entry.data.len());
        let wire = if entry.data.len() <= CHUNK_BYTES && lane != 2 {
            entry.data.clone()
        } else {
            let mut envelope = Message::new();
            envelope.set_gdesk_bulk_chunk(GDeskBulkChunk {
                transfer_id: entry.transfer,
                chunk_index: (entry.offset / CHUNK_BYTES) as u32,
                total_chunks: entry.data.len().div_ceil(CHUNK_BYTES) as u32,
                total_bytes: entry.data.len() as u32,
                payload: entry.data[entry.offset..end].to_vec().into(),
                ..Default::default()
            });
            envelope.write_to_bytes()?
        };
        entry.offset = end;
        self.scheduler.push_framed([Priority::P0, Priority::P1, Priority::P2][lane], wire, now);
        Ok(())
    }

    fn pop_ready(&mut self, now: Instant, allow_bulk: bool) -> ResultType<Option<Packet>> {
        let Some(packet) = self.scheduler.pop_ready(now, allow_bulk && self.bulk_ready()) else { return Ok(None); };
        let lane = lane(packet.priority);
        if self.outgoing[lane].front().is_some_and(|entry| entry.offset == entry.data.len()) {
            if let Some(entry) = self.outgoing[lane].pop_front() { self.queued_bytes -= entry.data.len(); }
        }
        self.prepare(lane, now)?;
        Ok(Some(packet))
    }

    #[cfg(test)]
    pub fn pop(&mut self, now: Instant) -> Option<Vec<u8>> {
        self.pop_ready(now, true).unwrap().map(|packet| packet.payload)
    }

    pub fn has_control(&self) -> bool { self.scheduler.has_control() }

    pub async fn send_one(&mut self, stream: &mut Stream, allow_bulk: bool) -> ResultType<Option<Priority>> {
        let Some(packet) = self.pop_ready(Instant::now(), allow_bulk)? else { return Ok(None); };
        let payload = if packet.priority == Priority::P2 {
            self.sent_credit = self.sent_credit.checked_add(1)
                .ok_or_else(|| hbb_common::anyhow::anyhow!("GDesk bulk credit exhausted"))?;
            let mut msg = Message::parse_from_bytes(&packet.payload)?;
            msg.mut_gdesk_bulk_chunk().credit_seq = self.sent_credit;
            msg.write_to_bytes()?
        } else { packet.payload };
        stream.send_raw(payload).await?;
        Ok(Some(packet.priority))
    }

    fn bulk_ready(&self) -> bool {
        self.sent_credit - self.acked_credit < BULK_WINDOW
    }

    pub async fn receive_from_peer(&mut self, msg: Message, now: Instant, stream: &mut Stream) -> ResultType<Option<Message>> {
        if let Some(message::Union::GdeskBulkAck(ack)) = &msg.union {
            if !self.negotiated || ack.credit_seq == 0 || ack.credit_seq > self.sent_credit {
                bail!("Invalid GDesk bulk acknowledgement");
            }
            self.acked_credit = self.acked_credit.max(ack.credit_seq);
            return Ok(None);
        }
        let credit = match &msg.union {
            Some(message::Union::GdeskBulkChunk(chunk)) => chunk.credit_seq,
            _ => 0,
        };
        let result = self.receive(msg, now)?;
        if credit != 0 {
            let mut ack = Message::new();
            ack.set_gdesk_bulk_ack(GDeskBulkAck { credit_seq: credit, ..Default::default() });
            stream.send(&ack).await?;
        }
        Ok(result)
    }

    pub fn receive(&mut self, msg: Message, now: Instant) -> ResultType<Option<Message>> {
        self.check(now)?;
        let Some(message::Union::GdeskBulkChunk(chunk)) = msg.union.as_ref() else { return Ok(Some(msg)); };
        if !self.negotiated { bail!("GDesk bulk received before negotiation"); }
        let total = chunk.total_bytes as usize;
        let index = chunk.chunk_index as usize;
        if chunk.transfer_id == 0 || total == 0 || total > MAX_MESSAGE
            || chunk.total_chunks as usize != total.div_ceil(CHUNK_BYTES)
            || index >= chunk.total_chunks as usize
            || chunk.payload.len() != (total - index * CHUNK_BYTES).min(CHUNK_BYTES)
        { bail!("Invalid GDesk bulk chunk bounds"); }
        if !self.incoming.contains_key(&chunk.transfer_id) {
            if index != 0 || self.incoming.len() >= MAX_TRANSFERS || self.reserved_bytes + total.next_power_of_two() > MAX_BUFFERED {
                bail!("GDesk bulk transfer/memory limit or missing first chunk");
            }
            self.reserved_bytes += total.next_power_of_two();
            self.incoming.insert(chunk.transfer_id, Assembly { total, next: 0, started: now, data: Vec::new() });
        }
        let assembly = self.incoming.get_mut(&chunk.transfer_id)
            .ok_or_else(|| hbb_common::anyhow::anyhow!("Missing GDesk assembly"))?;
        if assembly.total != total || assembly.next != chunk.chunk_index {
            bail!("GDesk bulk chunk out of sequence");
        }
        assembly.data.extend_from_slice(&chunk.payload);
        assembly.next += 1;
        assembly.started = now;
        if assembly.next != chunk.total_chunks { return Ok(None); }
        let assembly = self.incoming.remove(&chunk.transfer_id)
            .ok_or_else(|| hbb_common::anyhow::anyhow!("Missing completed GDesk assembly"))?;
        self.reserved_bytes -= assembly.total.next_power_of_two();
        let message = Message::parse_from_bytes(&assembly.data)?;
        if (total > CHUNK_BYTES && !bulk_business(&message)) || matches!(message.union,
            Some(message::Union::GdeskBulkChunk(_)) | Some(message::Union::GdeskBulkAck(_))
            | Some(message::Union::GdeskTransport(_))) { bail!("Invalid GDesk bulk business message"); }
        Ok(Some(message))
    }
}

fn lane(priority: Priority) -> usize {
    match priority { Priority::P0 => 0, Priority::P1 => 1, Priority::P2 => 2 }
}

fn bulk_business(msg: &Message) -> bool {
    match &msg.union {
        Some(message::Union::Clipboard(_)) | Some(message::Union::MultiClipboards(_))
        | Some(message::Union::CursorData(_)) => true,
        Some(message::Union::VideoFrame(vf)) => vf.union.is_some(),
        Some(message::Union::ScreenshotResponse(_)) => true,
        Some(message::Union::TerminalResponse(response)) => response.union.is_some(),
        Some(message::Union::PortForwardChannel(channel)) => channel.union.is_some(),
        Some(message::Union::Misc(misc)) => matches!(&misc.union, Some(misc::Union::ChatMessage(_))),
        Some(message::Union::FileResponse(fr)) => fr.union.is_some(),
        Some(message::Union::FileAction(action)) => matches!(&action.union, Some(file_action::Union::Receive(_))),
        Some(message::Union::Cliprdr(clip)) => matches!(&clip.union,
            Some(cliprdr::Union::FormatList(_)) | Some(cliprdr::Union::FormatDataResponse(_))
            | Some(cliprdr::Union::FileContentsResponse(_)) | Some(cliprdr::Union::Files(_))),
        _ => false,
    }
}

fn priority(msg: &Message) -> Priority {
    match &msg.union {
        Some(message::Union::VideoFrame(_)) => Priority::P1,
        Some(message::Union::Misc(misc)) => match &misc.union {
            Some(misc::Union::SwitchDisplay(_)) => Priority::P1,
            Some(misc::Union::GdeskAck(_)) | Some(misc::Union::VideoReceived(_)) => Priority::P0,
            _ if msg.compute_size() <= CHUNK_BYTES as u64 => Priority::P0,
            _ => Priority::P2,
        },
        Some(message::Union::CursorData(_)) | Some(message::Union::CursorPosition(_))
        | Some(message::Union::CursorId(_)) | Some(message::Union::TestDelay(_))
        | Some(message::Union::KeyEvent(_)) | Some(message::Union::MouseEvent(_))
        | Some(message::Union::PointerDeviceEvent(_)) => Priority::P0,
        _ => Priority::P2,
    }
}

pub(crate) async fn send_message(tx: &mut Transport, stream: &mut Stream, msg: &Message) -> ResultType<()> {
    if tx.is_active() || !tx.is_empty() { tx.enqueue(msg, Instant::now()) }
    else { stream.send(msg).await }
}

pub(crate) async fn send_raw(tx: &mut Transport, stream: &mut Stream, bytes: Vec<u8>) -> ResultType<()> {
    if tx.is_active() || !tx.is_empty() {
        let msg = Message::parse_from_bytes(&bytes)?;
        tx.enqueue(&msg, Instant::now())
    } else { stream.send_raw(bytes).await }
}

// The legacy helper owns Stream directly; keep its path unchanged for sessions without GDesk.
pub(crate) async fn handle_read_jobs(tx: &mut Transport, jobs: &mut Vec<fs::TransferJob>) -> ResultType<String> {
    for job in jobs.iter_mut().filter(|job| !job.is_last_job) {
        match job.init_data_stream_for_cm().await {
            Ok(Some((last_modified, file_size))) => {
                let mut response = FileResponse::new();
                response.set_digest(FileTransferDigest { id: job.id(), file_num: job.file_num(), last_modified, file_size,
                    is_resume: job.is_resume, ..Default::default() });
                let mut msg = Message::new();
                msg.set_file_response(response);
                tx.enqueue(&msg, Instant::now())?;
            }
            Ok(None) => {}
            Err(err) => tx.enqueue(&fs::new_error(job.id(), err, job.file_num()), Instant::now())?,
        }
    }
    let mut log = String::new();
    let mut finished = None;
    if let Some(job) = jobs.iter_mut().find(|job| !job.is_last_job) {
        let msg = match job.read().await {
            Err(err) => Some(fs::new_error(job.id(), err, job.file_num())),
            Ok(Some(block)) => Some(fs::new_block(block)),
            Ok(None) if job.job_completed() => {
                finished = Some(job.id());
                match job.job_error() {
                    Some(err) => {
                        log = fs::serialize_transfer_job(job, false, false, &err);
                        Some(fs::new_error(job.id(), err, job.file_num()))
                    }
                    None => {
                        log = fs::serialize_transfer_job(job, true, false, "");
                        Some(fs::new_done(job.id(), job.file_num()))
                    }
                }
            }
            Ok(None) => None,
        };
        if let Some(msg) = msg { tx.enqueue(&msg, Instant::now())?; }
    }
    if let Some(id) = finished { fs::remove_job(id, jobs); }
    Ok(log)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hbb_common::tokio::{self, net::{TcpListener, TcpStream}, time::{timeout, Duration}};

    fn clipboard(size: usize) -> Message {
        let mut msg = Message::new();
        msg.set_clipboard(Clipboard { content: vec![0x5a; size].into(), ..Default::default() });
        msg
    }

    #[tokio::test]
    async fn gdesk_bulk_sends_control_before_video_and_reports_actual_priority() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (socket, accepted) = tokio::join!(TcpStream::connect(addr), listener.accept());
        let mut sender = Stream::from(socket.unwrap(), addr);
        let (socket, addr) = accepted.unwrap();
        let mut receiver = Stream::from(socket, addr);
        let key = hbb_common::sodiumoxide::crypto::secretbox::Key([0x51; 32]);
        sender.set_key(key.clone());
        receiver.set_key(key);
        let mut tx = Transport::default();
        let mut rx = Transport::default();
        rx.negotiate(BULK_VERSION);
        let now = Instant::now();
        tx.enqueue(&clipboard(1), now).unwrap();
        let mut video = Message::new();
        video.set_video_frame(VideoFrame::new());
        tx.enqueue(&video, now).unwrap();
        let mut cursor = Message::new();
        cursor.set_cursor_data(CursorData { colors: vec![7; 20 * 1024].into(), ..Default::default() });
        tx.enqueue(&cursor, now).unwrap();
        assert!(tx.has_control());
        for _ in 0..2 {
            assert_eq!(tx.send_one(&mut sender, false).await.unwrap(), Some(Priority::P0));
        }
        assert!(!tx.has_control());
        assert!(tx.has_video());
        assert_eq!(tx.send_one(&mut sender, false).await.unwrap(), Some(Priority::P1));
        assert_eq!(tx.send_one(&mut sender, false).await.unwrap(), None);
        assert_eq!(tx.send_one(&mut sender, true).await.unwrap(), Some(Priority::P2));
        let mut received = Vec::new();
        for _ in 0..4 {
            let bytes = receiver.next_timeout(5000).await.unwrap().unwrap();
            if let Some(msg) = rx.receive(Message::parse_from_bytes(&bytes).unwrap(), now).unwrap() {
                received.push(msg);
            }
        }
        assert_eq!(received, vec![cursor, video, clipboard(1)]);
    }

    #[tokio::test]
    async fn gdesk_bulk_credit_bounds_background_and_keeps_control_ready() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (socket, accepted) = tokio::join!(TcpStream::connect(addr), listener.accept());
        let mut sender = Stream::from(socket.unwrap(), addr);
        let (socket, addr) = accepted.unwrap();
        let mut receiver = Stream::from(socket, addr);
        let mut tx = Transport::default();
        let mut rx = Transport::default();
        tx.negotiate(BULK_VERSION);
        rx.negotiate(BULK_VERSION);
        for _ in 0..6 { tx.enqueue(&clipboard(1), Instant::now()).unwrap(); }
        for _ in 0..BULK_WINDOW {
            assert_eq!(tx.send_one(&mut sender, true).await.unwrap(), Some(Priority::P2));
        }
        assert_eq!(tx.send_one(&mut sender, true).await.unwrap(), None);
        assert_eq!(tx.next_ready_at_with_bulk(true), None);
        let mut cursor = Message::new();
        cursor.set_cursor_id(42);
        tx.enqueue(&cursor, Instant::now()).unwrap();
        assert_eq!(tx.send_one(&mut sender, true).await.unwrap(), Some(Priority::P0));
        let bytes = receiver.next_timeout(5000).await.unwrap().unwrap();
        assert_eq!(rx.receive_from_peer(Message::parse_from_bytes(&bytes).unwrap(), Instant::now(), &mut receiver).await.unwrap(), Some(clipboard(1)));
        let bytes = sender.next_timeout(5000).await.unwrap().unwrap();
        tx.receive_from_peer(Message::parse_from_bytes(&bytes).unwrap(), Instant::now(), &mut sender).await.unwrap();
        assert_eq!(tx.send_one(&mut sender, true).await.unwrap(), Some(Priority::P2));
        let mut invalid = Message::new();
        invalid.set_gdesk_bulk_ack(GDeskBulkAck { credit_seq: u64::MAX, ..Default::default() });
        assert!(tx.receive_from_peer(invalid, Instant::now(), &mut sender).await.is_err());
    }

    #[test]
    fn gdesk_bulk_validates_bounds_and_reassembles_without_recursive_dispatch() {
        let now = Instant::now();
        let mut tx = Transport::default();
        tx.negotiate(BULK_VERSION);
        tx.set_active(true);
        let original = clipboard(50 * 1024 * 1024);
        tx.enqueue(&original, now).unwrap();
        let mut rx = Transport::default();
        rx.negotiate(BULK_VERSION);
        let mut result = None;
        while let Some(packet) = tx.pop(now) {
            assert!(packet.len() <= CHUNK_BYTES + 64);
            if let Some(msg) = rx.receive(Message::parse_from_bytes(&packet).unwrap(), now).unwrap() {
                result = Some(msg);
            }
        }
        assert_eq!(result, Some(original));
        let invalid = GDeskBulkChunk { transfer_id: 1, chunk_index: 0,
            total_chunks: u32::MAX, total_bytes: u32::MAX, payload: vec![1].into(), ..Default::default() };
        let mut msg = Message::new();
        msg.set_gdesk_bulk_chunk(invalid);
        assert!(rx.receive(msg.clone(), now).is_err());
        assert!(Transport::default().receive(msg, now).is_err());
    }

    #[test]
    fn gdesk_bulk_preserves_large_chat_screenshot_and_codec_switch() {
        let mut chat = Message::new();
        let mut misc = Misc::new();
        misc.set_chat_message(ChatMessage { text: "x".repeat(40 * 1024), ..Default::default() });
        chat.set_misc(misc);
        let mut screenshot = Message::new();
        screenshot.set_screenshot_response(ScreenshotResponse {
            data: vec![7; 40 * 1024].into(), ..Default::default()
        });
        let mut video = VideoFrame::new();
        video.set_vp9s(EncodedVideoFrames { frames: vec![EncodedVideoFrame {
            data: vec![9; 40 * 1024].into(), ..Default::default()
        }], ..Default::default() });
        let mut codec_switch = Message::new();
        codec_switch.set_video_frame(video);
        for original in [chat, screenshot, codec_switch] {
            let now = Instant::now();
            let mut tx = Transport::default();
            let mut rx = Transport::default();
            rx.negotiate(BULK_VERSION);
            tx.enqueue(&original, now).unwrap();
            let mut result = None;
            while let Some(wire) = tx.pop(now) {
                if let Some(message) = rx.receive(Message::parse_from_bytes(&wire).unwrap(), now).unwrap() {
                    result = Some(message);
                }
            }
            assert_eq!(result, Some(original));
        }
    }

    #[tokio::test]
    async fn gdesk_bulk_real_encrypted_socket_interleaves_clipboard_cursor_and_video() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (client, accepted) = tokio::join!(TcpStream::connect(addr), listener.accept());
        let mut sender = Stream::from(client.unwrap(), addr);
        let (socket, peer_addr) = accepted.unwrap();
        let mut receiver = Stream::from(socket, peer_addr);
        let key = hbb_common::sodiumoxide::crypto::secretbox::Key([0x42; 32]);
        sender.set_key(key.clone());
        receiver.set_key(key);
        let send = tokio::spawn(async move {
            let mut tx = Transport::default();
            tx.negotiate(BULK_VERSION);
            tx.set_active(true);
            send_message(&mut tx, &mut sender, &clipboard(20 * 1024 * 1024)).await.unwrap();
            let mut next_video = Instant::now();
            let mut seq = 0;
            while !tx.is_empty() || tx.sent_credit != tx.acked_credit {
                let now = Instant::now();
                if now >= next_video {
                    seq += 1;
                    let mut cursor = Message::new();
                    cursor.set_cursor_id(seq);
                    send_message(&mut tx, &mut sender, &cursor).await.unwrap();
                    let mut frames = EncodedVideoFrames::new();
                    frames.frames.push(EncodedVideoFrame { data: vec![seq as u8; 80 * 1024].into(), ..Default::default() });
                    let mut vf = VideoFrame::new();
                    vf.set_gdesks(frames);
                    let mut video = Message::new();
                    video.set_video_frame(vf);
                    send_message(&mut tx, &mut sender, &video).await.unwrap();
                    next_video = now + Duration::from_millis(20);
                }
                tx.send_one(&mut sender, true).await.unwrap();
                if let Ok(Some(Ok(bytes))) = timeout(Duration::from_millis(1), sender.next()).await {
                    let msg = Message::parse_from_bytes(&bytes).unwrap();
                    tx.receive_from_peer(msg, Instant::now(), &mut sender).await.unwrap();
                }
            }
            seq
        });
        let receive = async {
            let mut rx = Transport::default();
            rx.negotiate(BULK_VERSION);
            let mut cursor_count = 0;
            let mut video_count = 0;
            let mut intervals = Vec::new();
            let mut last_video = None;
            loop {
                let bytes = receiver.next().await.unwrap().unwrap();
                let msg = Message::parse_from_bytes(&bytes).unwrap();
                if let Some(msg) = rx.receive_from_peer(msg, Instant::now(), &mut receiver).await.unwrap() {
                    match msg.union {
                        Some(message::Union::CursorId(_)) => cursor_count += 1,
                        Some(message::Union::VideoFrame(_)) => {
                            let now = Instant::now();
                            if let Some(last) = last_video { intervals.push(now.duration_since(last)); }
                            last_video = Some(now);
                            video_count += 1;
                        }
                        Some(message::Union::Clipboard(clip)) => {
                            assert_eq!(clip.content.as_ref(), vec![0x5a; 20 * 1024 * 1024]);
                            break;
                        }
                        _ => panic!("unexpected message"),
                    }
                }
            }
            assert!(cursor_count >= 10 && video_count >= 10);
            intervals.sort();
            let p99 = intervals[intervals.len() * 99 / 100];
            eprintln!("real encrypted TCP: clipboard=20MiB cursor={cursor_count} video={video_count} P99={p99:?} max={:?}", intervals.last());
            assert!(p99 < Duration::from_millis(250));
        };
        timeout(Duration::from_secs(30), receive).await.unwrap();
        send.await.unwrap();
    }
}
