include!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../src/lib.rs"));

#[allow(unused_variables)]
mod acceptance {
use super::*;
use base::message_proto::*;
use hbb_common::{config::{Config, APP_NAME}, futures::StreamExt, protobuf::Message as _, rendezvous_proto::ConnType, tokio};
use crate::ui_session_interface::{InvokeUiSession, Session};
use crate::client::QualityStatus;
use std::sync::atomic::{AtomicUsize, Ordering};
static FRAME: AtomicUsize = AtomicUsize::new(0);
fn stamp() -> u128 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_micros() }
#[derive(Clone, Default)]
struct Observer;
impl InvokeUiSession for Observer {
fn set_cursor_data(&self, cd: CursorData)  { eprintln!("E2E cursor_shape {} {}", stamp(), cd.id); }
fn set_cursor_id(&self, id: String)  {  }
fn set_cursor_position(&self, cp: CursorPosition)  { eprintln!("E2E cursor {} {} {}", stamp(), cp.x, cp.y); }
fn set_display(&self, x: i32, y: i32, w: i32, h: i32, cursor_embedded: bool, scale: f64)  {  }
fn switch_display(&self, display: &SwitchDisplay)  {  }
fn set_peer_info(&self, peer_info: &PeerInfo)  { eprintln!("E2E peer_info {:?} displays={:?}", peer_info.platform, peer_info.displays); }
fn set_displays(&self, displays: &Vec<DisplayInfo>)  {  }
fn set_platform_additions(&self, data: &str)  {  }
fn on_connected(&self, conn_type: ConnType)  { eprintln!("E2E connected {:?}", conn_type); }
fn update_privacy_mode(&self)  {  }
fn set_permission(&self, name: &str, value: bool)  {  }
fn close_success(&self)  {  }
fn update_quality_status(&self, qs: QualityStatus)  { eprintln!("E2E quality {} {:?}", stamp(), qs); }
fn set_connection_type(&self, is_secured: bool, direct: bool, stream_type: &str)  { eprintln!("E2E transport secured={} direct={} type={}", is_secured, direct, stream_type); }
fn set_fingerprint(&self, fingerprint: String)  {  }
fn job_error(&self, id: i32, err: String, file_num: i32)  {  }
fn job_done(&self, id: i32, file_num: i32)  {  }
fn clear_all_jobs(&self)  {  }
fn new_message(&self, msg: String)  {  }
fn update_transfer_list(&self)  {  }
fn load_last_job(&self, cnt: i32, job_json: &str, auto_start: bool)  {  }
fn update_folder_files(
        &self,
        id: i32,
        entries: &Vec<FileEntry>,
        path: String,
        is_local: bool,
        only_count: bool,
    )  {  }
fn confirm_delete_files(&self, id: i32, i: i32, name: String)  {  }
fn override_file_confirm(
        &self,
        id: i32,
        file_num: i32,
        to: String,
        is_upload: bool,
        is_identical: bool,
    )  {  }
fn update_block_input_state(&self, on: bool)  {  }
fn job_progress(&self, id: i32, file_num: i32, speed: f64, finished_size: f64)  {  }
fn adapt_size(&self)  {  }
fn on_rgba(&self, display: usize, rgba: &mut scrap::ImageRgb)  { let n = FRAME.fetch_add(1, std::sync::atomic::Ordering::SeqCst); if n % 20 == 0 { eprintln!("E2E decoded {} frame={} bytes={}", stamp(), n, rgba.raw.len()); } if let Ok(p) = std::env::var("E2E_PIXELS") { std::fs::write(p, &rgba.raw).unwrap(); } }
fn msgbox(&self, msgtype: &str, title: &str, text: &str, link: &str, retry: bool)  { eprintln!("E2E msgbox {} {} {}", msgtype, title, text); }
fn cancel_msgbox(&self, tag: &str)  {  }
fn switch_back(&self, id: &str)  {  }
fn portable_service_running(&self, running: bool)  {  }
fn on_voice_call_started(&self)  {  }
fn on_voice_call_closed(&self, reason: &str)  {  }
fn on_voice_call_waiting(&self)  {  }
fn on_voice_call_incoming(&self)  {  }
fn get_rgba(&self, display: usize) -> *const u8 { std::ptr::null() }
fn next_rgba(&self, display: usize)  {  }
fn set_multiple_windows_session(&self, sessions: Vec<WindowsSession>)  {  }
fn set_current_display(&self, disp_idx: i32)  {  }
fn update_record_status(&self, start: bool)  {  }
fn update_empty_dirs(&self, _res: ReadEmptyDirsResponse)  {  }
fn printer_request(&self, id: i32, path: String)  {  }
fn handle_screenshot_resp(&self, sid: String, msg: String)  {  }
fn handle_terminal_response(&self, response: TerminalResponse)  {  }
}
struct Logger;
impl hbb_common::log::Log for Logger {
fn enabled(&self, _: &hbb_common::log::Metadata) -> bool { true }
fn log(&self, r: &hbb_common::log::Record) { if r.level() <= hbb_common::log::Level::Debug || r.target() == "gdesk::udp" { eprintln!("{} {} {}", stamp(), r.level(), r.args()); } }
fn flush(&self) {}
}
static LOGGER: Logger = Logger;
