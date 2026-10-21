//! A direct path between the page and this Mac beside the relay: a WebRTC
//! data channel the page offers once it is connected. The offer and answer
//! travel inside the session, so through the relay they are sealed end to
//! end and carry each side's DTLS fingerprint; the relay cannot put itself
//! in the middle. Once the channel opens, the session's interactive replies
//! and the desktop view's pictures take it, and page messages arriving on it
//! join the relay's. Files stay on the relay. If the channel closes, both
//! go back to the relay.
//!
//! Every relay hop costs a round trip to the relay's region; a direct path
//! on the same network or across a friendly NAT skips it (maintainer,
//! 2026-10-21, asking for the simplest direct path to test).
use crate::queue;
use parking_lot::Mutex;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use str0m::change::SdpOffer;
use str0m::channel::ChannelId;
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcConfig};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

/// One message for the page, split into pieces no larger than this: a
/// keyframe is larger than some browsers take in one data channel message.
const PIECE: usize = 16 * 1024;
/// Data the channel may hold unsent before the session waits, so a slow path
/// pushes back on the desktop's frame window instead of piling up here.
const MAX_BUFFERED: usize = 1024 * 1024;
/// How long a direct path may take to open before the page's offer is
/// dropped and the relay carries on alone.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// A message for the page: text (JSON) or a desktop picture.
type Outgoing = (bool, Vec<u8>);
type Slot = Arc<Mutex<Option<mpsc::Sender<Outgoing>>>>;

#[derive(serde::Deserialize)]
struct Offer {
    #[serde(rename = "type")]
    kind: String,
    sdp: String,
}

/// Put a direct path beside the session's own: returns the inbox, outbox and
/// frames the session uses instead.
pub(crate) fn wrap(
    mut inbox: mpsc::Receiver<String>,
    outbox: queue::Sender<String>,
    frames: mpsc::Sender<Vec<u8>>,
) -> (
    mpsc::Receiver<String>,
    queue::Sender<String>,
    mpsc::Sender<Vec<u8>>,
) {
    let (inner_in, session_in) = mpsc::channel::<String>(64);
    let (session_out, inner_out) = queue::channel::<String>(64);
    let (session_frames, mut inner_frames) = mpsc::channel::<Vec<u8>>(4);
    let direct: Slot = Arc::new(Mutex::new(None));

    // The page's messages: an offer starts a direct path, the rest go on.
    {
        let direct = direct.clone();
        let outbox = outbox.clone();
        tokio::spawn(async move {
            while let Some(text) = inbox.recv().await {
                if text.contains("\"rtc_offer\"") {
                    if let Ok(offer) = serde_json::from_str::<Offer>(&text) {
                        if offer.kind == "rtc_offer" {
                            answer(&offer.sdp, &outbox, &inner_in, &direct).await;
                            continue;
                        }
                    }
                }
                if inner_in.send(text).await.is_err() {
                    break;
                }
            }
        });
    }

    let queue::Receiver {
        mut interactive,
        mut bulk,
        ..
    } = inner_out;
    {
        let direct = direct.clone();
        let outbox = outbox.clone();
        tokio::spawn(async move {
            while let Some(text) = interactive.recv().await {
                let text = match send_direct(&direct, (false, text.into_bytes())).await {
                    Ok(()) => continue,
                    Err(bytes) => String::from_utf8(bytes).unwrap_or_default(),
                };
                if outbox.send(text).await.is_err() {
                    break;
                }
            }
        });
    }
    {
        let outbox = outbox.clone();
        tokio::spawn(async move {
            while let Some(text) = bulk.recv().await {
                if outbox.send_bulk(text).await.is_err() {
                    break;
                }
            }
        });
    }
    tokio::spawn(async move {
        while let Some(frame) = inner_frames.recv().await {
            let frame = match send_direct(&direct, (true, frame)).await {
                Ok(()) => continue,
                Err(frame) => frame,
            };
            if frames.send(frame).await.is_err() {
                break;
            }
        }
    });
    (session_in, session_out, session_frames)
}

/// Send on the direct path if one is open; hands the message back if not.
async fn send_direct(direct: &Slot, (binary, bytes): Outgoing) -> Result<(), Vec<u8>> {
    let Some(tx) = direct.lock().clone() else {
        return Err(bytes);
    };
    tx.send((binary, bytes))
        .await
        .map_err(|mpsc::error::SendError((_, bytes))| bytes)
}

/// Answer the page's offer through the session and start the direct path.
async fn answer(
    sdp: &str,
    outbox: &queue::Sender<String>,
    inbox: &mpsc::Sender<String>,
    direct: &Slot,
) {
    let started = match start(sdp).await {
        Ok(started) => started,
        Err(e) => {
            log::info!("kaku-remote p2p: no direct path: {e:#}");
            return;
        }
    };
    let (rtc, sockets, answer) = started;
    let reply = serde_json::json!({ "type": "rtc_answer", "sdp": answer }).to_string();
    if outbox.send(reply).await.is_err() {
        return;
    }
    // A new offer replaces the old path.
    *direct.lock() = None;
    tokio::spawn(run(rtc, sockets, inbox.clone(), direct.clone()));
}

async fn start(sdp: &str) -> anyhow::Result<(Rtc, Sockets, String)> {
    let offer = SdpOffer::from_sdp_string(sdp)?;
    let sockets = Sockets::bind(&local_ips()).await?;
    let mut rtc = RtcConfig::new()
        .set_crypto_provider(Arc::new(str0m::crypto::from_feature_flags()))
        .build(Instant::now());
    for local in sockets.locals() {
        rtc.add_local_candidate(Candidate::host(local, "udp")?);
    }
    let answer = rtc.sdp_api().accept_offer(offer)?;
    Ok((rtc, sockets, answer.to_sdp_string()))
}

/// The Mac's IPv4 addresses a peer may reach directly. Left out: loopback,
/// link-local, and 198.18.0.0/15, where proxy apps in TUN mode (Stash,
/// Clash) put a fake interface that no peer can reach; the route to the
/// internet picked that one on the maintainer's Mac. Tailscale's 100.64/10
/// stays, as a path that works across networks.
fn local_ips() -> Vec<IpAddr> {
    let mut ips = Vec::new();
    // SAFETY: getifaddrs hands back a list that stays valid until
    // freeifaddrs, and every pointer read is checked for null first.
    unsafe {
        let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut head) != 0 {
            return ips;
        }
        let mut at = head;
        while !at.is_null() {
            let ifa = &*at;
            let up = ifa.ifa_flags & libc::IFF_UP as u32 != 0
                && ifa.ifa_flags & libc::IFF_LOOPBACK as u32 == 0;
            if up
                && !ifa.ifa_addr.is_null()
                && i32::from((*ifa.ifa_addr).sa_family) == libc::AF_INET
            {
                let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                if reachable(ip) && !ips.contains(&IpAddr::V4(ip)) {
                    ips.push(IpAddr::V4(ip));
                }
            }
            at = ifa.ifa_next;
        }
        libc::freeifaddrs(head);
    }
    ips.truncate(4);
    ips
}

fn reachable(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    let fake = a == 198 && b & 0xfe == 18;
    !(ip.is_loopback() || ip.is_link_local() || ip.is_unspecified() || fake)
}

/// One UDP socket per address, and the datagrams they hear.
struct Sockets {
    sockets: Vec<Arc<UdpSocket>>,
    heard: mpsc::Receiver<(SocketAddr, SocketAddr, Vec<u8>)>,
    readers: Vec<tokio::task::JoinHandle<()>>,
}

impl Sockets {
    async fn bind(ips: &[IpAddr]) -> anyhow::Result<Self> {
        let (tx, heard) = mpsc::channel(256);
        let mut sockets = Vec::new();
        let mut readers = Vec::new();
        for &ip in ips {
            let Ok(socket) = UdpSocket::bind(SocketAddr::new(ip, 0)).await else {
                continue;
            };
            let socket = Arc::new(socket);
            let local = socket.local_addr()?;
            let reader = socket.clone();
            let tx = tx.clone();
            readers.push(tokio::spawn(async move {
                let mut buf = vec![0u8; 2048];
                while let Ok((n, source)) = reader.recv_from(&mut buf).await {
                    if tx.send((local, source, buf[..n].to_vec())).await.is_err() {
                        break;
                    }
                }
            }));
            sockets.push(socket);
        }
        anyhow::ensure!(!sockets.is_empty(), "no local address");
        Ok(Self {
            sockets,
            heard,
            readers,
        })
    }

    fn locals(&self) -> Vec<SocketAddr> {
        self.sockets
            .iter()
            .filter_map(|s| s.local_addr().ok())
            .collect()
    }

    fn send(&self, from: SocketAddr, to: SocketAddr, bytes: &[u8]) {
        let socket = self
            .sockets
            .iter()
            .find(|s| s.local_addr().ok() == Some(from))
            .unwrap_or(&self.sockets[0]);
        let _ = socket.try_send_to(bytes, to);
    }
}

impl Drop for Sockets {
    fn drop(&mut self) {
        for reader in &self.readers {
            reader.abort();
        }
    }
}

/// Pieces of one message: a flag byte (1 last, 2 text) and up to `PIECE`
/// bytes in all.
fn pieces(binary: bool, bytes: &[u8]) -> Vec<Vec<u8>> {
    let text = if binary { 0 } else { 2 };
    let chunks: Vec<&[u8]> = if bytes.is_empty() {
        vec![&[][..]]
    } else {
        bytes.chunks(PIECE - 1).collect()
    };
    let last = chunks.len() - 1;
    chunks
        .into_iter()
        .enumerate()
        .map(|(i, chunk)| {
            let mut piece = Vec::with_capacity(chunk.len() + 1);
            piece.push(text | u8::from(i == last));
            piece.extend_from_slice(chunk);
            piece
        })
        .collect()
}

async fn run(mut rtc: Rtc, mut sockets: Sockets, inbox: mpsc::Sender<String>, direct: Slot) {
    let began = Instant::now();
    let (tx, mut rx) = mpsc::channel::<Outgoing>(8);
    let mut channel: Option<ChannelId> = None;
    let mut pending: std::collections::VecDeque<Vec<u8>> = Default::default();
    let mut sent = 0u64;
    let end = |direct: &Slot, tx: &mpsc::Sender<Outgoing>| {
        let mut slot = direct.lock();
        if slot.as_ref().is_some_and(|s| s.same_channel(tx)) {
            *slot = None;
        }
    };
    loop {
        // Hand the channel what it can take.
        if let Some(id) = channel {
            while let Some(piece) = pending.front() {
                let Some(mut ch) = rtc.channel(id) else { break };
                if ch.buffered_amount() > MAX_BUFFERED || !matches!(ch.write(true, piece), Ok(true))
                {
                    break;
                }
                sent += piece.len() as u64;
                pending.pop_front();
            }
        }
        let deadline = loop {
            match rtc.poll_output() {
                Ok(Output::Timeout(at)) => break at,
                Ok(Output::Transmit(t)) => sockets.send(t.source, t.destination, &t.contents),
                Ok(Output::Event(event)) => match event {
                    Event::ChannelOpen(id, _) => {
                        channel = Some(id);
                        *direct.lock() = Some(tx.clone());
                        log::info!(
                            "kaku-remote p2p: direct path open in {} ms",
                            began.elapsed().as_millis()
                        );
                    }
                    Event::ChannelData(data) => {
                        if let Ok(text) = String::from_utf8(data.data) {
                            if inbox.send(text).await.is_err() {
                                end(&direct, &tx);
                                return;
                            }
                        }
                    }
                    Event::ChannelClose(_)
                    | Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                        end(&direct, &tx);
                        log::info!(
                            "kaku-remote p2p: direct path closed after {} s, {} KB sent",
                            began.elapsed().as_secs(),
                            sent / 1024
                        );
                        return;
                    }
                    _ => {}
                },
                Err(e) => {
                    end(&direct, &tx);
                    log::info!("kaku-remote p2p: direct path failed: {e}");
                    return;
                }
            }
        };
        if channel.is_none() && began.elapsed() > CONNECT_TIMEOUT {
            log::info!(
                "kaku-remote p2p: no direct path within {} s",
                CONNECT_TIMEOUT.as_secs()
            );
            return;
        }
        let wait = deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_secs(1));
        let room = pending.is_empty();
        tokio::select! {
            got = sockets.heard.recv() => {
                let Some((local, source, bytes)) = got else { return };
                let Ok(contents) = bytes.as_slice().try_into() else { continue };
                let input = Input::Receive(Instant::now(), Receive {
                    proto: Protocol::Udp,
                    source,
                    destination: local,
                    contents,
                });
                if rtc.handle_input(input).is_err() {
                    end(&direct, &tx);
                    return;
                }
            }
            msg = rx.recv(), if room => {
                let Some((binary, bytes)) = msg else { return };
                pending.extend(pieces(binary, &bytes));
            }
            _ = tokio::time::sleep(wait) => {
                if rtc.handle_input(Input::Timeout(Instant::now())).is_err() {
                    end(&direct, &tx);
                    return;
                }
            }
            _ = inbox.closed() => {
                end(&direct, &tx);
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_is_cut_into_flagged_pieces() {
        let big = vec![7u8; PIECE * 2];
        let parts = pieces(true, &big);
        assert_eq!(parts.len(), 3);
        assert!(parts.iter().all(|p| p.len() <= PIECE));
        assert_eq!(parts.iter().map(|p| p[0]).collect::<Vec<_>>(), [0, 0, 1]);
        assert_eq!(parts.iter().map(|p| p.len() - 1).sum::<usize>(), big.len());
        assert_eq!(pieces(false, b"{}"), [vec![3, b'{', b'}']]);
        assert_eq!(pieces(false, b""), [vec![3]]);
    }

    /// Drive one side until `done` says so or ten seconds pass.
    async fn drive(
        rtc: &mut Rtc,
        socket: &UdpSocket,
        mut on: impl FnMut(&mut Rtc, Event) -> bool,
    ) -> bool {
        let local = socket.local_addr().unwrap();
        let until = Instant::now() + Duration::from_secs(10);
        let mut buf = vec![0u8; 2048];
        while Instant::now() < until {
            let deadline = loop {
                match rtc.poll_output().unwrap() {
                    Output::Timeout(at) => break at,
                    Output::Transmit(t) => {
                        let _ = socket.try_send_to(&t.contents, t.destination);
                    }
                    Output::Event(event) => {
                        if on(rtc, event) {
                            return true;
                        }
                    }
                }
            };
            let wait = deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(50));
            tokio::select! {
                got = socket.recv_from(&mut buf) => {
                    let (n, source) = got.unwrap();
                    let input = Input::Receive(Instant::now(), Receive {
                        proto: Protocol::Udp,
                        source,
                        destination: local,
                        contents: buf[..n].try_into().unwrap(),
                    });
                    rtc.handle_input(input).unwrap();
                }
                _ = tokio::time::sleep(wait) => {
                    rtc.handle_input(Input::Timeout(Instant::now())).unwrap();
                }
            }
        }
        false
    }

    #[tokio::test]
    async fn an_offered_channel_carries_frames_and_page_messages() {
        let Some(&ip) = local_ips().first() else {
            return;
        };
        let (page_in, inbox) = mpsc::channel::<String>(8);
        let (outbox, mut relay_out) = queue::channel::<String>(8);
        let (frames, mut relay_frames) = mpsc::channel::<Vec<u8>>(4);
        let (session_in, session_out, session_frames) = wrap(inbox, outbox, frames);
        let mut session_in = session_in;

        // The page's side, offering one channel.
        let socket = UdpSocket::bind(SocketAddr::new(ip, 0)).await.unwrap();
        let mut page = RtcConfig::new()
            .set_crypto_provider(Arc::new(str0m::crypto::from_feature_flags()))
            .build(Instant::now());
        page.add_local_candidate(Candidate::host(socket.local_addr().unwrap(), "udp").unwrap());
        let mut change = page.sdp_api();
        let id = change.add_channel("kaku".into());
        let (offer, pending) = change.apply().unwrap();
        let offer = serde_json::json!({ "type": "rtc_offer", "sdp": offer.to_sdp_string() });
        page_in.send(offer.to_string()).await.unwrap();

        // The answer comes back on the relay, never to the session.
        let answer = relay_out.recv().await.unwrap();
        let answer: serde_json::Value = serde_json::from_str(&answer).unwrap();
        assert_eq!(answer["type"], "rtc_answer");
        let answer =
            str0m::change::SdpAnswer::from_sdp_string(answer["sdp"].as_str().unwrap()).unwrap();
        page.sdp_api().accept_answer(pending, answer).unwrap();
        assert!(
            drive(&mut page, &socket, |_, e| matches!(
                e,
                Event::ChannelOpen(..)
            ))
            .await
        );

        // Kaku's side opens a moment later; wait until it takes the path.
        let mut frame = vec![0u8; 40 * 1024];
        frame[1] = 9;
        let mut got = Vec::new();
        let sent = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            session_frames.send(frame).await.unwrap();
            session_out
                .send("{\"type\":\"pong\"}".into())
                .await
                .unwrap();
        });
        let mut texts = Vec::new();
        assert!(
            drive(&mut page, &socket, |_, e| {
                if let Event::ChannelData(data) = e {
                    got.extend_from_slice(&data.data[1..]);
                    if data.data[0] & 1 == 1 {
                        texts.push((data.data[0] & 2 == 2, std::mem::take(&mut got)));
                    }
                }
                texts.len() == 2
            })
            .await
        );
        sent.await.unwrap();
        assert!(
            relay_frames.try_recv().is_err(),
            "the frame went by the relay"
        );
        assert!(!texts[0].0);
        assert_eq!(texts[0].1.len(), 40 * 1024);
        assert_eq!(texts[0].1[1], 9);
        assert_eq!(texts[1], (true, b"{\"type\":\"pong\"}".to_vec()));

        // What the page sends on the channel reaches the session.
        assert!(page
            .channel(id)
            .unwrap()
            .write(false, b"{\"type\":\"ping\"}")
            .unwrap());
        let heard = tokio::select! {
            heard = session_in.recv() => heard,
            _ = drive(&mut page, &socket, |_, _| false) => None,
        };
        assert_eq!(heard.as_deref(), Some("{\"type\":\"ping\"}"));
    }

    /// A real browser on the other side: `tests/p2p_probe.cjs` writes the
    /// page's offer to `$KAKU_P2P_DIR/offer.sdp` and reads the answer.
    #[tokio::test]
    #[ignore]
    async fn browser_probe() {
        let dir = std::path::PathBuf::from(std::env::var("KAKU_P2P_DIR").unwrap());
        let offer_path = dir.join("offer.sdp");
        while !offer_path.exists() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        let sdp = std::fs::read_to_string(&offer_path).unwrap();
        let (page_in, inbox) = mpsc::channel::<String>(8);
        let (outbox, mut relay_out) = queue::channel::<String>(8);
        let (frames, _relay_frames) = mpsc::channel::<Vec<u8>>(4);
        let (mut session_in, session_out, session_frames) = wrap(inbox, outbox, frames);
        let offer = serde_json::json!({ "type": "rtc_offer", "sdp": sdp });
        page_in.send(offer.to_string()).await.unwrap();
        let answer = tokio::time::timeout(Duration::from_secs(5), relay_out.recv())
            .await
            .unwrap()
            .unwrap();
        let answer: serde_json::Value = serde_json::from_str(&answer).unwrap();
        std::fs::write(dir.join("answer.sdp"), answer["sdp"].as_str().unwrap()).unwrap();
        let until = Instant::now() + Duration::from_secs(20);
        while let Ok(Some(text)) = tokio::time::timeout_at(until.into(), session_in.recv()).await {
            let mut frame = vec![0u8; 300 * 1024];
            frame[1] = 7;
            session_frames.send(frame).await.unwrap();
            session_out
                .send(text.replace("ping", "pong"))
                .await
                .unwrap();
        }
    }
}
