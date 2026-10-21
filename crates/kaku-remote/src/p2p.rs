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

/// The open direct path: its number, which the page's messages about it
/// carry, and where the session's messages for it go.
#[derive(Clone)]
struct Path {
    id: u64,
    tx: mpsc::Sender<Outgoing>,
}
type Slot = Arc<Mutex<Option<Path>>>;

/// The page's messages about the direct path: `rtc_offer` starts one,
/// `rtc_ready` says the page sends on it from here on, `rtc_close` says the
/// page gave it up.
#[derive(serde::Deserialize)]
struct Control {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    sdp: String,
    #[serde(default)]
    id: u64,
}

fn control(text: &str) -> Option<Control> {
    if !text.contains("\"rtc_") {
        return None;
    }
    serde_json::from_str::<Control>(text)
        .ok()
        .filter(|c| c.kind.starts_with("rtc_"))
}

/// Told to the helper whenever the pictures move to the other path: those
/// still on the old one may never arrive, and its round trip says nothing
/// about the new one.
const PATH_CHANGED: &str = r#"{"type":"desktop_keyframe","path_changed":true}"#;

/// The last message a lane sends by the relay before it moves to path `id`,
/// so the page takes what comes on the path only after what was already on
/// the way through the relay.
fn ready_text(id: u64) -> String {
    serde_json::json!({ "type": "rtc_ready", "lane": "text", "id": id }).to_string()
}

fn ready_frame(id: u64) -> Vec<u8> {
    let header = serde_json::json!({ "type": "rtc_ready", "lane": "frames", "id": id }).to_string();
    let mut out = Vec::with_capacity(5 + header.len());
    out.push(0);
    out.extend_from_slice(&(header.len() as u32).to_be_bytes());
    out.extend_from_slice(header.as_bytes());
    out
}

/// Messages the page sent on the path before its `rtc_ready` came through
/// the relay are held at most this many; past it the page is not following
/// the order and they go on.
const MAX_HELD: usize = 1024;

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

    // The page's messages, from the relay and from the path, in one order:
    // what came on the path waits for the page's `rtc_ready` on the relay,
    // after everything it sent there before.
    {
        let direct = direct.clone();
        let outbox = outbox.clone();
        let (from_path, mut on_path) = mpsc::channel::<(u64, String)>(64);
        tokio::spawn(async move {
            let mut current: Option<(u64, tokio::task::JoinHandle<()>)> = None;
            let mut last_id = 0u64;
            let mut ready = false;
            let mut held: Vec<String> = Vec::new();
            loop {
                let (text, from) = tokio::select! {
                    got = inbox.recv() => match got {
                        Some(text) => (text, None),
                        None => break,
                    },
                    Some((id, text)) = on_path.recv() => (text, Some(id)),
                };
                let current_id = current.as_ref().map(|(id, _)| *id);
                if let Some(id) = from {
                    if current_id != Some(id) {
                        continue;
                    }
                    if !ready && held.len() < MAX_HELD {
                        held.push(text);
                        continue;
                    }
                } else if let Some(c) = control(&text) {
                    match c.kind.as_str() {
                        "rtc_offer" => {
                            // A new offer replaces the old path.
                            stop(&mut current, &direct);
                            last_id += 1;
                            ready = false;
                            held.clear();
                            let path =
                                answer(last_id, &c.sdp, &outbox, &inner_in, &from_path, &direct);
                            if let Some(task) = path.await {
                                current = Some((last_id, task));
                            }
                        }
                        "rtc_ready" if current_id == Some(c.id) && !ready => {
                            ready = true;
                            for text in held.drain(..) {
                                if inner_in.send(text).await.is_err() {
                                    break;
                                }
                            }
                        }
                        "rtc_close" if current_id == Some(c.id) => {
                            stop(&mut current, &direct);
                            log::info!("kaku-remote p2p: the page closed the direct path");
                            let _ = inner_in.try_send(PATH_CHANGED.into());
                        }
                        _ => {}
                    }
                    continue;
                }
                if inner_in.send(text).await.is_err() {
                    break;
                }
            }
            stop(&mut current, &direct);
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
            let mut on = None;
            while let Some(text) = interactive.recv().await {
                let path = direct.lock().clone();
                let text = match path {
                    Some(path) => {
                        if on != Some(path.id) {
                            if outbox.send(ready_text(path.id)).await.is_err() {
                                break;
                            }
                            on = Some(path.id);
                        }
                        match path.tx.send((false, text.into_bytes())).await {
                            Ok(()) => continue,
                            Err(mpsc::error::SendError((_, bytes))) => {
                                on = None;
                                String::from_utf8(bytes).unwrap_or_default()
                            }
                        }
                    }
                    None => {
                        on = None;
                        text
                    }
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
        let mut on = None;
        while let Some(frame) = inner_frames.recv().await {
            let path = direct.lock().clone();
            let frame = match path {
                Some(path) => {
                    if on != Some(path.id) {
                        if frames.send(ready_frame(path.id)).await.is_err() {
                            break;
                        }
                        on = Some(path.id);
                    }
                    match path.tx.send((true, frame)).await {
                        Ok(()) => continue,
                        Err(mpsc::error::SendError((_, frame))) => {
                            on = None;
                            frame
                        }
                    }
                }
                None => {
                    on = None;
                    frame
                }
            };
            if frames.send(frame).await.is_err() {
                break;
            }
        }
    });
    (session_in, session_out, session_frames)
}

/// End the current path, if any, and stop sending on it.
fn stop(current: &mut Option<(u64, tokio::task::JoinHandle<()>)>, direct: &Slot) {
    let Some((id, task)) = current.take() else {
        return;
    };
    task.abort();
    let mut slot = direct.lock();
    if slot.as_ref().is_some_and(|p| p.id == id) {
        *slot = None;
    }
}

/// Answer the page's offer through the session and start path `id`.
async fn answer(
    id: u64,
    sdp: &str,
    outbox: &queue::Sender<String>,
    inbox: &mpsc::Sender<String>,
    from_path: &mpsc::Sender<(u64, String)>,
    direct: &Slot,
) -> Option<tokio::task::JoinHandle<()>> {
    let started = match start(sdp).await {
        Ok(started) => started,
        Err(e) => {
            log::info!("kaku-remote p2p: no direct path: {e:#}");
            let closed = serde_json::json!({ "type": "rtc_closed", "id": id }).to_string();
            let _ = outbox.send(closed).await;
            return None;
        }
    };
    let (rtc, sockets, answer) = started;
    let reply = serde_json::json!({ "type": "rtc_answer", "sdp": answer, "id": id }).to_string();
    outbox.send(reply).await.ok()?;
    let ends = Ends {
        id,
        outbox: outbox.clone(),
        inbox: inbox.clone(),
        from_path: from_path.clone(),
        direct: direct.clone(),
    };
    Some(tokio::spawn(run(rtc, sockets, ends)))
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

/// What a path needs beyond its connection: where the page's messages on it
/// go, and who to tell when it ends.
struct Ends {
    id: u64,
    outbox: queue::Sender<String>,
    inbox: mpsc::Sender<String>,
    from_path: mpsc::Sender<(u64, String)>,
    direct: Slot,
}

/// Run path `ends.id` until it ends, then go back to the relay and tell the
/// page, which otherwise keeps sending on a channel nobody reads until its
/// browser notices, about 30 seconds later.
async fn run(rtc: Rtc, sockets: Sockets, ends: Ends) {
    let began = Instant::now();
    let mut sent = 0u64;
    let mut opened = false;
    let why = carry(rtc, sockets, &ends, began, &mut sent, &mut opened).await;
    {
        let mut slot = ends.direct.lock();
        if slot.as_ref().is_some_and(|p| p.id == ends.id) {
            *slot = None;
        }
    }
    if opened {
        log::info!(
            "kaku-remote p2p: direct path closed after {} s, {} KB sent: {why}",
            began.elapsed().as_secs(),
            sent / 1024
        );
        let _ = ends.inbox.send(PATH_CHANGED.into()).await;
    } else {
        log::info!("kaku-remote p2p: no direct path: {why}");
    }
    let closed = serde_json::json!({ "type": "rtc_closed", "id": ends.id }).to_string();
    let _ = ends.outbox.send(closed).await;
}

/// The path's loop; returns why it ended.
async fn carry(
    mut rtc: Rtc,
    mut sockets: Sockets,
    ends: &Ends,
    began: Instant,
    sent: &mut u64,
    opened: &mut bool,
) -> String {
    let (tx, mut rx) = mpsc::channel::<Outgoing>(8);
    let mut channel: Option<ChannelId> = None;
    let mut pending: std::collections::VecDeque<Vec<u8>> = Default::default();
    loop {
        // Hand the channel what it can take.
        if let Some(id) = channel {
            while let Some(piece) = pending.front() {
                let Some(mut ch) = rtc.channel(id) else { break };
                if ch.buffered_amount() > MAX_BUFFERED || !matches!(ch.write(true, piece), Ok(true))
                {
                    break;
                }
                *sent += piece.len() as u64;
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
                        *opened = true;
                        *ends.direct.lock() = Some(Path {
                            id: ends.id,
                            tx: tx.clone(),
                        });
                        let _ = ends.inbox.send(PATH_CHANGED.into()).await;
                        log::info!(
                            "kaku-remote p2p: direct path open in {} ms",
                            began.elapsed().as_millis()
                        );
                    }
                    Event::ChannelData(data) => {
                        if let Ok(text) = String::from_utf8(data.data) {
                            if ends.from_path.send((ends.id, text)).await.is_err() {
                                return "the session ended".into();
                            }
                        }
                    }
                    Event::ChannelClose(_) => return "the channel closed".into(),
                    // The page offers again when it can; a path that comes
                    // back on its own would need the lanes ordered again.
                    Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                        return "the connection dropped".into()
                    }
                    _ => {}
                },
                Err(e) => return e.to_string(),
            }
        };
        if channel.is_none() && began.elapsed() > CONNECT_TIMEOUT {
            return format!("none within {} s", CONNECT_TIMEOUT.as_secs());
        }
        let wait = deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_secs(1));
        let room = pending.is_empty();
        tokio::select! {
            got = sockets.heard.recv() => {
                let Some((local, source, bytes)) = got else { return "the sockets closed".into() };
                let Ok(contents) = bytes.as_slice().try_into() else { continue };
                let input = Input::Receive(Instant::now(), Receive {
                    proto: Protocol::Udp,
                    source,
                    destination: local,
                    contents,
                });
                if let Err(e) = rtc.handle_input(input) {
                    return e.to_string();
                }
            }
            msg = rx.recv(), if room => {
                let Some((binary, bytes)) = msg else { return "the session ended".into() };
                pending.extend(pieces(binary, &bytes));
            }
            _ = tokio::time::sleep(wait) => {
                if let Err(e) = rtc.handle_input(Input::Timeout(Instant::now())) {
                    return e.to_string();
                }
            }
            _ = ends.inbox.closed() => return "the session ended".into(),
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

    /// The session's ends of a wrapped pipe, and the page's side of an
    /// open path.
    struct Opened {
        page: Rtc,
        socket: UdpSocket,
        channel: ChannelId,
        path: u64,
        page_in: mpsc::Sender<String>,
        relay_out: queue::Receiver<String>,
        relay_frames: mpsc::Receiver<Vec<u8>>,
        session_in: mpsc::Receiver<String>,
        session_out: queue::Sender<String>,
        session_frames: mpsc::Sender<Vec<u8>>,
    }

    async fn open() -> Option<Opened> {
        let &ip = local_ips().first()?;
        let (page_in, inbox) = mpsc::channel::<String>(8);
        let (outbox, mut relay_out) = queue::channel::<String>(8);
        let (frames, relay_frames) = mpsc::channel::<Vec<u8>>(8);
        let (mut session_in, session_out, session_frames) = wrap(inbox, outbox, frames);

        // The page's side, offering one channel.
        let socket = UdpSocket::bind(SocketAddr::new(ip, 0)).await.unwrap();
        let mut page = RtcConfig::new()
            .set_crypto_provider(Arc::new(str0m::crypto::from_feature_flags()))
            .build(Instant::now());
        page.add_local_candidate(Candidate::host(socket.local_addr().unwrap(), "udp").unwrap());
        let mut change = page.sdp_api();
        let channel = change.add_channel("kaku".into());
        let (offer, pending) = change.apply().unwrap();
        let offer = serde_json::json!({ "type": "rtc_offer", "sdp": offer.to_sdp_string() });
        page_in.send(offer.to_string()).await.unwrap();

        // The answer comes back on the relay, never to the session.
        let answer = relay_out.recv().await.unwrap();
        let answer: serde_json::Value = serde_json::from_str(&answer).unwrap();
        assert_eq!(answer["type"], "rtc_answer");
        let path = answer["id"].as_u64().unwrap();
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
        // Kaku's side opens a moment later and tells the helper.
        let told = tokio::select! {
            told = session_in.recv() => told,
            _ = drive(&mut page, &socket, |_, _| false) => None,
        };
        assert_eq!(told.as_deref(), Some(PATH_CHANGED));
        Some(Opened {
            page,
            socket,
            channel,
            path,
            page_in,
            relay_out,
            relay_frames,
            session_in,
            session_out,
            session_frames,
        })
    }

    /// What the page hears on the path until it has `count` messages.
    async fn heard(o: &mut Opened, count: usize) -> Vec<(bool, Vec<u8>)> {
        let mut got = Vec::new();
        let mut whole = Vec::new();
        assert!(
            drive(&mut o.page, &o.socket, |_, e| {
                if let Event::ChannelData(data) = e {
                    got.extend_from_slice(&data.data[1..]);
                    if data.data[0] & 1 == 1 {
                        whole.push((data.data[0] & 2 == 2, std::mem::take(&mut got)));
                    }
                }
                whole.len() == count
            })
            .await
        );
        whole
    }

    #[tokio::test]
    async fn an_offered_channel_carries_frames_and_page_messages() {
        let Some(mut o) = open().await else { return };
        let mut frame = vec![0u8; 40 * 1024];
        frame[1] = 9;
        o.session_frames.send(frame).await.unwrap();
        o.session_out
            .send("{\"type\":\"pong\"}".into())
            .await
            .unwrap();
        let texts = heard(&mut o, 2).await;
        assert!(!texts[0].0);
        assert_eq!(texts[0].1.len(), 40 * 1024);
        assert_eq!(texts[0].1[1], 9);
        assert_eq!(texts[1], (true, b"{\"type\":\"pong\"}".to_vec()));

        // Each lane's last word on the relay says it moved, so the page
        // takes the path's messages after those still on the way.
        let marker = o.relay_frames.try_recv().unwrap();
        assert_eq!(marker, ready_frame(o.path));
        assert!(
            o.relay_frames.try_recv().is_err(),
            "the frame went by the relay"
        );
        assert_eq!(o.relay_out.recv().await.unwrap(), ready_text(o.path));

        // What the page sends on the channel waits for its own `rtc_ready`
        // on the relay, then reaches the session after what came before it.
        let ping = b"{\"type\":\"ping\"}";
        assert!(o
            .page
            .channel(o.channel)
            .unwrap()
            .write(false, ping)
            .unwrap());
        let early = tokio::select! {
            got = o.session_in.recv() => got,
            _ = async {
                drive(&mut o.page, &o.socket, |_, _| false).await;
            } => None,
            _ = tokio::time::sleep(Duration::from_millis(500)) => None,
        };
        assert_eq!(early, None, "taken before the page's rtc_ready");
        o.page_in
            .send("{\"type\":\"key\",\"data\":\"a\"}".into())
            .await
            .unwrap();
        let ready = serde_json::json!({ "type": "rtc_ready", "id": o.path }).to_string();
        o.page_in.send(ready).await.unwrap();
        assert_eq!(
            o.session_in.recv().await.as_deref(),
            Some("{\"type\":\"key\",\"data\":\"a\"}")
        );
        assert_eq!(
            o.session_in.recv().await.as_deref(),
            Some("{\"type\":\"ping\"}")
        );
    }

    #[tokio::test]
    async fn a_path_the_page_gives_up_sends_everything_by_the_relay_again() {
        let Some(mut o) = open().await else { return };
        let close = serde_json::json!({ "type": "rtc_close", "id": o.path }).to_string();
        o.page_in.send(close).await.unwrap();
        assert_eq!(o.session_in.recv().await.as_deref(), Some(PATH_CHANGED));
        o.session_out
            .send("{\"type\":\"pong\"}".into())
            .await
            .unwrap();
        assert_eq!(
            o.relay_out.recv().await.as_deref(),
            Some("{\"type\":\"pong\"}")
        );
        o.session_frames.send(vec![0, 1]).await.unwrap();
        assert_eq!(o.relay_frames.recv().await, Some(vec![0, 1]));
    }

    #[tokio::test]
    async fn a_path_that_ends_on_its_own_is_told_to_the_page() {
        let Some(mut o) = open().await else { return };
        o.page.direct_api().close_data_channel(o.channel);
        let closed = tokio::select! {
            closed = o.relay_out.recv() => closed,
            _ = drive(&mut o.page, &o.socket, |_, _| false) => None,
        };
        let closed: serde_json::Value = serde_json::from_str(&closed.unwrap()).unwrap();
        assert_eq!(closed["type"], "rtc_closed");
        assert_eq!(closed["id"], o.path);
        assert_eq!(o.session_in.recv().await.as_deref(), Some(PATH_CHANGED));
        o.session_out
            .send("{\"type\":\"pong\"}".into())
            .await
            .unwrap();
        assert_eq!(
            o.relay_out.recv().await.as_deref(),
            Some("{\"type\":\"pong\"}")
        );
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
        // The probe sends nothing by the relay: its path messages go on at once.
        let ready = serde_json::json!({ "type": "rtc_ready", "id": answer["id"] });
        page_in.send(ready.to_string()).await.unwrap();
        let until = Instant::now() + Duration::from_secs(20);
        while let Ok(Some(text)) = tokio::time::timeout_at(until.into(), session_in.recv()).await {
            if text == PATH_CHANGED {
                continue;
            }
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
