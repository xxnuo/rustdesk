use base::message_proto::{Message, VoiceCallRequest, VoiceCallResponse};
use hbb_common::get_time;
use scrap::CodecFormat;
use std::collections::HashMap;

#[derive(Debug, Default)]
pub struct QualityStatus {
    pub speed: Option<String>,
    pub fps: HashMap<usize, i32>,
    pub delay: Option<i32>,
    pub target_bitrate: Option<i32>,
    pub codec_format: Option<CodecFormat>,
    pub chroma: Option<String>,
    pub gdesk_photon_latency: Option<String>,
    pub gdesk_codec_us: Option<String>,
    pub gdesk_p99_jitter: Option<String>,
    pub gdesk_cache_hit_rate: Option<String>,
    pub gdesk_qos_light: Option<String>,
    pub gdesk_realtime_bandwidth: Option<String>,
}

#[cfg(feature = "gdesk")]
impl QualityStatus {
    pub fn set_gdesk_metrics(
        &mut self,
        metrics: &scrap::gdesk_codec::GDeskOverlayMetrics,
        rtt_ms: Option<u32>,
        render_queue_depth: usize,
    ) {
        let estimated_ms = metrics.estimated_photon_latency_ms(rtt_ms.unwrap_or_default(), render_queue_depth);
        self.gdesk_photon_latency = Some(format!("{estimated_ms:.2} ms"));
        self.gdesk_codec_us = Some(format!("{}/{} us", metrics.encode_us, metrics.decode_us));
        self.gdesk_p99_jitter = Some(format!("{:.2} ms", metrics.frame_p99_jitter_ms));
        self.gdesk_cache_hit_rate = Some(format!("{:.1}%", metrics.cache_hit_ratio_pct));
        self.gdesk_qos_light = Some(metrics.qos_light.to_owned());
        self.gdesk_realtime_bandwidth = Some(format!("{} kbps", metrics.current_realtime_kbps()));
    }
}

#[inline]
pub fn new_voice_call_request(is_connect: bool) -> Message {
    let mut req = VoiceCallRequest::new();
    req.is_connect = is_connect;
    req.req_timestamp = get_time();
    let mut msg = Message::new();
    msg.set_voice_call_request(req);
    msg
}

#[inline]
pub fn new_voice_call_response(request_timestamp: i64, accepted: bool) -> Message {
    let mut resp = VoiceCallResponse::new();
    resp.accepted = accepted;
    resp.req_timestamp = request_timestamp;
    resp.ack_timestamp = get_time();
    let mut msg = Message::new();
    msg.set_voice_call_response(resp);
    msg
}
