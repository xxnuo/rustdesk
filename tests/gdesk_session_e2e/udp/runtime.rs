pub fn run() -> hbb_common::ResultType<()> {
    hbb_common::log::set_logger(&LOGGER).unwrap();
    hbb_common::log::set_max_level(hbb_common::log::LevelFilter::Trace);
    *APP_NAME.write().unwrap() = "RustDeskRFC2UdpAcceptance".into();
    hbb_common::sodiumoxide::init().unwrap();
    let args: Vec<String> = std::env::args().collect();
    assert!(Config::file().starts_with(std::env::current_dir()?));
    // Populate key/config before choosing the private identity; no user config is mounted writable.
    let _ = Config::get_key_pair();
    Config::set_id(if args[1] == "server" { "876543210" } else { "876543211" });
    Config::set_nat_type(1);
    Config::set_option("custom-rendezvous-server".into(), "127.0.0.1:29870".into());
    Config::set_option("gdesk-udp-datagrams".into(), "Y".into());
    for (name, value) in [("enable-udp-punch", "Y"), ("enable-tcp-punch", "N"),
        ("enable-ipv6-punch", "N"), ("enable-webrtc", "N")] {
        hbb_common::config::LocalConfig::set_option(name.into(), value.into());
    }
    eprintln!("E2E role={} config={:?} id={} ipc={} HOME={:?}", args[1], Config::file(), Config::get_id(), Config::ipc_path("_cm"), std::env::var("HOME"));
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().worker_threads(4).build()?;
    rt.block_on(async {
        match args[1].as_str() {
            "rendezvous" => rendezvous().await,
            "server" => {
                Config::set_option("approve-mode".into(), "click".into());
                assert_eq!(Config::get_option("approve-mode"), "click");
                let mut listener = crate::ipc::new_listener("_cm").await?;
                tokio::spawn(async move {
                    while let Some(Ok(stream)) = listener.next().await {
                        tokio::spawn(async move {
                            let mut cm = crate::ipc::Connection::new(stream);
                            while let Ok(Some(data)) = cm.next().await {
                                if let crate::ipc::Data::Login { id, authorized, .. } = data {
                                    eprintln!("E2E CM login id={id} authorized={authorized}");
                                    if !authorized { cm.send(&crate::ipc::Data::Authorize).await.unwrap(); }
                                }
                            }
                        });
                    }
                });
                let server = crate::server::new();
                std::fs::write("server-ready", "ready")?;
                crate::rendezvous_mediator::RendezvousMediator::start_udp(server, "127.0.0.1:29870".into()).await
            }
            "client" => {
                let key = std::fs::read_to_string("rendezvous-key")?;
                let session: Session<Observer> = Session {
                    server_keyboard_enabled: std::sync::Arc::new(std::sync::RwLock::new(true)),
                    server_clipboard_enabled: std::sync::Arc::new(std::sync::RwLock::new(true)),
                    ..Default::default()
                };
                session.lc.write().unwrap().initialize(format!("876543210@127.0.0.1:29870?key={key}"),
                    ConnType::DEFAULT_CONN, None, false, None, None, None, "");
                session.set_option("codec-preference".into(), "gdesk".into());
                session.toggle_option("show-remote-cursor".into());
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                *session.sender.write().unwrap() = Some(tx.clone());
                let login = tx.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    let _ = login.send(crate::client::Data::Login(("".into(), "".into(), "".into(), false)));
                });
                let close = tx.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(90)).await;
                    let _ = close.send(crate::client::Data::Close);
                });
                let mut remote = crate::client::io_loop::Remote::new(session, rx, tx);
                eprintln!("E2E entering production Remote::io_loop");
                remote.io_loop(&key, "", 0).await;
                eprintln!("E2E Remote::io_loop ended decoded_frames={}", FRAME.load(Ordering::SeqCst));
                Ok(())
            }
            _ => panic!("unknown role"),
        }
    })
}

async fn rendezvous() -> hbb_common::ResultType<()> {
    use hbb_common::{rendezvous_proto as rv, sodiumoxide::crypto::sign, AddrMangle};
    use std::{net::SocketAddr, sync::Arc};
    let (root_pk, root_sk) = sign::gen_keypair();
    std::fs::write("rendezvous-key", hbb_common::base64::encode(root_pk.0))?;
    let udp = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:29870").await?);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:29870").await?;
    let registered = Arc::new(tokio::sync::Mutex::new(None::<(SocketAddr, String, Vec<u8>)>));
    let pending = Arc::new(tokio::sync::Mutex::new(None::<(tokio::sync::oneshot::Sender<SocketAddr>, u16)>));
    let (u, reg, pend) = (udp.clone(), registered.clone(), pending.clone());
    tokio::spawn(async move {
        let mut buf = vec![0; 65536];
        loop {
            let (n, source) = u.recv_from(&mut buf).await.unwrap();
            let msg = rv::RendezvousMessage::parse_from_bytes(&buf[..n]).unwrap();
            let mut response = rv::RendezvousMessage::new();
            match msg.union {
                Some(rv::rendezvous_message::Union::RegisterPk(pk)) => {
                    eprintln!("E2E RV register_pk id={} addr={source}", pk.id);
                    *reg.lock().await = Some((source, pk.id, pk.pk.to_vec()));
                    response.set_register_pk_response(rv::RegisterPkResponse::new());
                    std::fs::write("registered", "ready").unwrap();
                }
                Some(rv::rendezvous_message::Union::RegisterPeer(_)) => {
                    response.set_register_peer_response(rv::RegisterPeerResponse {
                        request_pk: reg.lock().await.is_none(), ..Default::default()
                    });
                }
                Some(rv::rendezvous_message::Union::TestNatRequest(_)) => {
                    response.set_test_nat_response(rv::TestNatResponse { port: source.port() as i32, ..Default::default() });
                    eprintln!("E2E RV NAT observed={source}");
                }
                Some(rv::rendezvous_message::Union::PunchHoleSent(sent)) => {
                    eprintln!("E2E RV PunchHoleSent id={} source={source}", sent.id);
                    if let Some((tx, _)) = pend.lock().await.take() { let _ = tx.send(source); }
                    continue;
                }
                _ => continue,
            }
            u.send_to(&response.write_to_bytes().unwrap(), source).await.unwrap();
        }
    });
    std::fs::write("rendezvous-ready", "ready")?;
    loop {
        let (socket, addr) = listener.accept().await?;
        let (u, reg, pend, sk) = (udp.clone(), registered.clone(), pending.clone(), root_sk.clone());
        tokio::spawn(async move {
            let local = socket.local_addr().unwrap();
            let mut stream = hbb_common::Stream::from(socket, local);
            while let Some(Ok(bytes)) = stream.next().await {
                let msg = rv::RendezvousMessage::parse_from_bytes(&bytes).unwrap();
                if let Some(rv::rendezvous_message::Union::PunchHoleRequest(request)) = msg.union {
                    eprintln!("E2E RV PunchHoleRequest id={} udp_port={} peer={addr}", request.id, request.udp_port);
                    assert!(request.udp_port > 0 && !request.force_relay && request.webrtc_sdp_offer.is_empty());
                    let (server_reg, server_id, server_pk) = reg.lock().await.clone().unwrap();
                    assert_eq!(request.id, server_id);
                    let proxy_client: SocketAddr = "127.0.0.1:29871".parse().unwrap();
                    let proxy_server: SocketAddr = "127.0.0.1:29872".parse().unwrap();
                    std::fs::write("client-udp-address", SocketAddr::new(addr.ip(), request.udp_port as u16).to_string()).unwrap();
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    *pend.lock().await = Some((tx, proxy_server.port()));
                    let mut punch = rv::RendezvousMessage::new();
                    punch.set_punch_hole(rv::PunchHole {
                        socket_addr: AddrMangle::encode(proxy_server).into(),
                        udp_port: proxy_server.port() as i32,
                        nat_type: rv::NatType::ASYMMETRIC.into(),
                        ..Default::default()
                    });
                    u.send_to(&punch.write_to_bytes().unwrap(), server_reg).await.unwrap();
                    let server_addr = tokio::time::timeout(std::time::Duration::from_secs(5), rx).await.unwrap().unwrap();
                    std::fs::write("server-udp-address", server_addr.to_string()).unwrap();
                    let identity = rv::IdPk { id: server_id, pk: server_pk.into(), ..Default::default() };
                    let mut response = rv::RendezvousMessage::new();
                    response.set_punch_hole_response(rv::PunchHoleResponse {
                        socket_addr: AddrMangle::encode(proxy_client).into(),
                        pk: sign::sign(&identity.write_to_bytes().unwrap(), &sk).into(),
                        is_udp: true,
                        union: Some(rv::punch_hole_response::Union::NatType(rv::NatType::ASYMMETRIC.into())),
                        ..Default::default()
                    });
                    stream.send(&response).await.unwrap();
                    eprintln!("E2E RV relayed signed identity; production peers must perform UDP/KCP handshake");
                }
            }
        });
    }
}
}
fn main() { if let Err(e) = acceptance::run() { eprintln!("E2E fatal {e:?}"); std::process::exit(1); } }
