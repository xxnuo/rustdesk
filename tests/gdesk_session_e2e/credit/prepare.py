from pathlib import Path
import re, shutil
root = Path(__file__).resolve().parents[3]
out = root / '.tmp/gdesk-session-e2e/credit'
out.mkdir(parents=True, exist_ok=True)
s = (root / 'Cargo.toml').read_text()
s = re.sub(r'\[lib\].*?(?=\n\[)', '', s, flags=re.S)
s = re.sub(r'\[\[bin\]\].*?(?=\n\[)', '', s, flags=re.S)
s = re.sub(r'\[workspace\].*?(?=\n\[)', '[workspace]\n', s, flags=re.S)
s = s.replace('name = "rustdesk"', 'name = "rfc2-session-e2e"', 1).replace('default-run = "rustdesk"', 'autobins = false').replace('build= "build.rs"', 'build = false')
s = re.sub(r'path\s*=\s*"(libs/[^\"]+)"', lambda m: 'path = "' + str(root / m[1]) + '"', s)
s += '\n[[bin]]\nname = "session-e2e"\npath = "session.rs"\n'
(out / 'Cargo.toml').write_text(s)
shutil.copyfile(root / 'Cargo.lock', out / 'Cargo.lock')
trait = (root / 'src/ui_session_interface.rs').read_text().split('pub trait InvokeUiSession:')[1].split('\n}\n')[0]
trait = trait[trait.index('{') + 1:]
methods = []
for m in re.finditer(r'fn\s+(\w+)\s*\((.*?)\)\s*(->\s*[^;{]+)?\s*(?:;|\{[^}]*\})', trait, re.S):
    name, args, ret = m.groups()
    if name in ['clipboard', 'on_texture', 'is_multi_ui_session']: continue
    body = ''
    if ret: body = 'std::ptr::null()' if '*const' in ret else 'Default::default()'
    overrides = {
      'set_peer_info': 'eprintln!("E2E peer_info {:?} displays={:?}", peer_info.platform, peer_info.displays);',
      'set_cursor_position': 'eprintln!("E2E cursor {} {} {}", stamp(), cp.x, cp.y);',
      'set_cursor_data': 'eprintln!("E2E cursor_shape {} {}", stamp(), cd.id);',
      'update_quality_status': 'eprintln!("E2E quality {} {:?}", stamp(), qs);',
      'set_connection_type': 'eprintln!("E2E transport secured={} direct={} type={}", is_secured, direct, stream_type);',
      'msgbox': 'eprintln!("E2E msgbox {} {} {}", msgtype, title, text);',
      'on_connected': 'eprintln!("E2E connected {:?}", conn_type);',
      'on_rgba': 'let n = FRAME.fetch_add(1, std::sync::atomic::Ordering::SeqCst); if n % 20 == 0 { eprintln!("E2E decoded {} frame={} bytes={}", stamp(), n, rgba.raw.len()); } if let Ok(p) = std::env::var("E2E_PIXELS") { std::fs::write(p, &rgba.raw).unwrap(); }',
    }
    body = overrides.get(name, body)
    methods.append(f'fn {name}({args}) {ret or ""} {{ {body} }}')
head = 'include!("' + str(root / 'src/lib.rs') + '");\n'
observer = '''
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
'''+ '\n'.join(methods) + '''
}
struct Logger;
impl hbb_common::log::Log for Logger {
fn enabled(&self, _: &hbb_common::log::Metadata) -> bool { true }
fn log(&self, r: &hbb_common::log::Record) { if r.level() <= hbb_common::log::Level::Debug { eprintln!("{} {} {}", stamp(), r.level(), r.args()); } }
fn flush(&self) {}
}
static LOGGER: Logger = Logger;
pub fn run() -> hbb_common::ResultType<()> {
hbb_common::log::set_logger(&LOGGER).unwrap();
hbb_common::log::set_max_level(hbb_common::log::LevelFilter::Debug);
*APP_NAME.write().unwrap() = "RustDeskRFC2Acceptance".into();
eprintln!("E2E config={:?} ipc={} display={:?}", Config::file(), Config::ipc_path("_cm"), std::env::var("DISPLAY"));
assert!(Config::file().starts_with(std::env::current_dir()?));
hbb_common::sodiumoxide::init().unwrap();
Config::set_id(if std::env::args().nth(1).as_deref() == Some("server") { "876543210" } else { "876543211" });
let args: Vec<String> = std::env::args().collect();
let rt = tokio::runtime::Builder::new_multi_thread().enable_all().worker_threads(4).build()?;
rt.block_on(async {
if args[1] == "server" {
Config::set_option("approve-mode".into(), "click".into());
assert_eq!(Config::get_option("approve-mode"), "click");
let mut cm = crate::ipc::new_listener("_cm").await?;
tokio::spawn(async move {
while let Some(Ok(stream)) = cm.next().await {
tokio::spawn(async move {
let mut cm = crate::ipc::Connection::new(stream);
while let Ok(Some(data)) = cm.next().await {
if let crate::ipc::Data::Login {id, authorized, ..} = data {
eprintln!("E2E CM login id={} authorized={}", id, authorized);
if !authorized { cm.send(&crate::ipc::Data::Authorize).await.unwrap(); }
// shortcut: injected by the real CM RawMessage API, OS clipboard ingestion is a separate check.
let trigger = std::path::Path::new("inject");
while !trigger.exists() { tokio::time::sleep(std::time::Duration::from_millis(100)).await; }
let content = std::fs::read("clipboard.txt").unwrap();
assert_eq!(content.len(), 20 * 1024 * 1024);
let mut msg = Message::new();
msg.set_clipboard(Clipboard {content: content.into(), compress:false, ..Default::default()});
eprintln!("E2E clipboard_enqueue {} bytes={}", stamp(), msg.compute_size());
cm.send(&crate::ipc::Data::RawMessage(msg.write_to_bytes().unwrap())).await.unwrap();
}
}
});
}
});
let server = crate::server::new();
let listener = tokio::net::TcpListener::bind("127.0.0.1:29876").await?;
std::fs::write("ready", b"ready")?;
loop {
let (stream, addr) = listener.accept().await?;
let local = stream.local_addr()?;
let server = server.clone();
tokio::spawn(async move {
eprintln!("E2E calling create_tcp_connection {}", stamp());
let res = crate::create_tcp_connection(server, hbb_common::Stream::from(stream, local), addr, false, Default::default()).await;
eprintln!("E2E Connection::start exited {:?}", res);
});
}
} else {
let session: Session<Observer> = Session {
server_keyboard_enabled: std::sync::Arc::new(std::sync::RwLock::new(true)),
server_clipboard_enabled: std::sync::Arc::new(std::sync::RwLock::new(true)),
..Default::default()
};
session.lc.write().unwrap().initialize("127.0.0.1:29877".into(), hbb_common::rendezvous_proto::ConnType::DEFAULT_CONN, None, false, None, None, None, "");
session.set_option("codec-preference".into(), "gdesk".into());
if !session.get_toggle_option("show-remote-cursor".into()) { session.toggle_option("show-remote-cursor".into()); }
let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
*session.sender.write().unwrap() = Some(tx.clone());
let login = tx.clone();
tokio::spawn(async move { tokio::time::sleep(std::time::Duration::from_secs(2)).await; login.send(crate::client::Data::Login(("".into(), "".into(), "".into(), false))).unwrap(); });
let close = tx.clone();
tokio::spawn(async move { tokio::time::sleep(std::time::Duration::from_secs(90)).await; close.send(crate::client::Data::Close).unwrap(); });
let mut remote = crate::client::io_loop::Remote::new(session, rx, tx);
eprintln!("E2E entering Remote::io_loop");
remote.io_loop("", "", 0).await;
eprintln!("E2E Remote::io_loop exited decoded_frames={}", FRAME.load(Ordering::SeqCst));
Ok(())
}
})
}
}
fn main() { if let Err(e) = acceptance::run() { eprintln!("E2E fatal {e:?}"); std::process::exit(1); } }
'''
(out / 'session.rs').write_text(head + observer)
print('Prepared exact-source harness; production files unchanged')
