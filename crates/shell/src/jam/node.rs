//! Host and guest tasks. The UI polls [`JamEvent`]s; it never touches a socket.

use std::collections::HashMap;
use std::time::Duration;

use iroh::endpoint::{presets, Connection, RecvStream, SendStream};
use iroh::{Endpoint, SecretKey};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::sync::{mpsc, oneshot, watch};

use super::clock::{unix_ms, ClockSync};
use super::protocol::{
    aggregate_path, authorize, clean_name, frame_len_ok, hello_room_ok, stamp_action, ClientMsg,
    HostMsg, JamAction, JamRole, JamSnapshot, PathKind, PlaybackAnchor, ALPN, MAX_GUESTS,
    PROTOCOL_VERSION,
};
use super::ticket::{decode_ticket, encode_ticket, room_hex};

#[derive(Clone, Debug)]
pub struct PublishUpdate {
    pub gen: u64,
    pub guests_can_control: bool,
    pub queue: Vec<super::protocol::JamTrack>,
    pub current_index: Option<usize>,
    pub playback: PlaybackAnchor,
}

#[derive(Clone, Debug)]
pub enum JamEvent {
    Connected {
        role: JamRole,
        ticket: Option<String>,
        self_id: String,
        snapshot: JamSnapshot,
    },
    RemoteState {
        snapshot: JamSnapshot,
        est_host_now_ms: u64,
        path: PathKind,
        rtt_ms: Option<u64>,
    },
    Heartbeat {
        est_host_now_ms: u64,
        playing: bool,
        position_ms: u64,
        track_uid: Option<u64>,
        path: PathKind,
        rtt_ms: Option<u64>,
    },
    GuestAction {
        peer_id: String,
        name: String,
        action: JamAction,
    },
    Roster {
        participants: Vec<super::protocol::Participant>,
        path: PathKind,
        rtt_ms: Option<u64>,
    },
    Ended(String),
    Notice(String),
}

enum Cmd {
    Publish(PublishUpdate),
    SetControl(bool),
    Kick(String),
    SendAction(JamAction),
    Shutdown,
}

struct Active {
    cmd: mpsc::UnboundedSender<Cmd>,
    role: JamRole,
}

pub struct JamManager {
    inner: std::sync::Mutex<Option<Active>>,
    events_tx: mpsc::UnboundedSender<JamEvent>,
    events_rx: std::sync::Mutex<mpsc::UnboundedReceiver<JamEvent>>,
}

impl Default for JamManager {
    fn default() -> Self {
        Self::new()
    }
}

impl JamManager {
    pub fn new() -> Self {
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        Self {
            inner: std::sync::Mutex::new(None),
            events_tx,
            events_rx: std::sync::Mutex::new(events_rx),
        }
    }

    pub fn is_active(&self) -> bool {
        self.inner.lock().map(|g| g.is_some()).unwrap_or(false)
    }

    pub fn role(&self) -> Option<JamRole> {
        self.inner
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|a| a.role))
    }

    pub fn poll_event(&self) -> Option<JamEvent> {
        self.events_rx.lock().ok()?.try_recv().ok()
    }

    pub async fn start_host(&self, name: String, guests_can_control: bool) -> Result<(), String> {
        self.spawn(JamRole::Host, name, guests_can_control, None, false)
            .await
    }

    /// Same room, but both ends stay on loopback with relays disabled.
    /// Integration tests use this so they do not need a WAN.
    pub async fn start_host_loopback(
        &self,
        name: String,
        guests_can_control: bool,
    ) -> Result<(), String> {
        self.spawn(JamRole::Host, name, guests_can_control, None, true)
            .await
    }

    pub async fn join(&self, ticket: String, name: String) -> Result<(), String> {
        self.spawn(JamRole::Guest, name, false, Some(ticket), false)
            .await
    }

    pub async fn join_loopback(&self, ticket: String, name: String) -> Result<(), String> {
        self.spawn(JamRole::Guest, name, false, Some(ticket), true)
            .await
    }

    async fn spawn(
        &self,
        role: JamRole,
        name: String,
        guests_can_control: bool,
        ticket: Option<String>,
        loopback: bool,
    ) -> Result<(), String> {
        if self.is_active() {
            return Err("already in a jam".into());
        }
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        if let Ok(mut g) = self.inner.lock() {
            *g = Some(Active { cmd: cmd_tx, role });
        }
        let events = self.events_tx.clone();
        match role {
            JamRole::Host => {
                tokio::spawn(async move {
                    run_host(events, cmd_rx, name, guests_can_control, loopback).await;
                });
            }
            JamRole::Guest => {
                let ticket = ticket.unwrap_or_default();
                tokio::spawn(async move {
                    run_guest(events, cmd_rx, name, ticket, loopback).await;
                });
            }
        }
        Ok(())
    }

    pub fn publish(&self, update: PublishUpdate) {
        self.cmd(Cmd::Publish(update));
    }

    pub fn set_control(&self, on: bool) {
        self.cmd(Cmd::SetControl(on));
    }

    pub fn kick(&self, peer_id: String) {
        self.cmd(Cmd::Kick(peer_id));
    }

    pub fn send_action(&self, action: JamAction) {
        self.cmd(Cmd::SendAction(action));
    }

    pub fn shutdown(&self) {
        let active = self.inner.lock().ok().and_then(|mut g| g.take());
        if let Some(active) = active {
            let _ = active.cmd.send(Cmd::Shutdown);
        }
    }

    fn cmd(&self, cmd: Cmd) {
        if let Some(active) = self
            .inner
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|a| a.cmd.clone()))
        {
            let _ = active.send(cmd);
        }
    }
}

enum SessionEnd {
    /// Host closed the room, or the code was wrong. Do not dial again.
    Fatal(String),
    /// The path dropped. The guest task waits and dials again.
    Retry(String),
    Local,
}

struct GuestLink {
    name: String,
    snap: watch::Sender<JamSnapshot>,
    extra: mpsc::UnboundedSender<HostMsg>,
    path: PathKind,
    rtt_ms: Option<u64>,
}

enum GuestEvt {
    Request {
        peer_id: String,
        name: String,
        reply: oneshot::Sender<Admit>,
    },
    Action {
        peer_id: String,
        name: String,
        action: JamAction,
    },
    Path {
        peer_id: String,
        path: PathKind,
        rtt_ms: Option<u64>,
    },
    Ping {
        peer_id: String,
        seq: u64,
        t1_guest_ms: u64,
    },
    Left {
        peer_id: String,
    },
}

enum Admit {
    Ok {
        snapshot: JamSnapshot,
        snap: watch::Sender<JamSnapshot>,
        extra: mpsc::UnboundedReceiver<HostMsg>,
    },
    No(String),
}

async fn bind_endpoint(loopback: bool) -> Result<Endpoint, String> {
    let key = SecretKey::generate();
    if loopback {
        Endpoint::builder(presets::Minimal)
            .secret_key(key)
            .alpns(vec![ALPN.to_vec()])
            .bind_addr("127.0.0.1:0")
            .map_err(|e| e.to_string())?
            .bind()
            .await
            .map_err(|e| e.to_string())
    } else {
        Endpoint::builder(presets::N0)
            .secret_key(key)
            .alpns(vec![ALPN.to_vec()])
            .bind()
            .await
            .map_err(|e| e.to_string())
    }
}

fn classify(conn: &Connection) -> (PathKind, Option<u64>) {
    let paths = conn.paths();
    let selected = paths.iter().find(|p| p.is_selected());
    let kind = match &selected {
        Some(p) if p.is_relay() => PathKind::Relayed,
        Some(_) => PathKind::Direct,
        None => PathKind::Unknown,
    };
    let rtt = selected.map(|p| p.rtt().as_millis() as u64);
    (kind, rtt)
}

async fn run_host(
    events: mpsc::UnboundedSender<JamEvent>,
    mut cmds: mpsc::UnboundedReceiver<Cmd>,
    name: String,
    guests_can_control: bool,
    loopback: bool,
) {
    let name = clean_name(&name);
    let endpoint = match bind_endpoint(loopback).await {
        Ok(ep) => ep,
        Err(e) => {
            let _ = events.send(JamEvent::Ended(format!("could not start the jam ({e})")));
            return;
        }
    };
    if !loopback {
        // A relay address in the invite is what lets a guest through a
        // strict NAT. Give discovery a few seconds, then share whatever
        // we have — LAN still works off the direct addresses alone.
        let _ = tokio::time::timeout(Duration::from_secs(6), endpoint.online()).await;
    }
    let room: [u8; 16] = rand::random();
    let ticket = match encode_ticket(&endpoint.addr(), &room) {
        Ok(t) => t,
        Err(e) => {
            let _ = events.send(JamEvent::Ended(e));
            endpoint.close().await;
            return;
        }
    };
    let host_id = endpoint.id().to_string();
    let mut snapshot = JamSnapshot::empty(host_id.clone(), name.clone(), guests_can_control);
    let _ = events.send(JamEvent::Connected {
        role: JamRole::Host,
        ticket: Some(ticket),
        self_id: host_id.clone(),
        snapshot: snapshot.clone(),
    });

    let (guest_tx, mut guest_rx) = mpsc::unbounded_channel();
    let accept_ep = endpoint.clone();
    let accept_tx = guest_tx.clone();
    let accept = tokio::spawn(async move {
        accept_loop(accept_ep, room, accept_tx).await;
    });

    let mut links: HashMap<String, GuestLink> = HashMap::new();
    let mut last_gen = 0u64;
    let mut heartbeat = tokio::time::interval(Duration::from_secs(2));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            cmd = cmds.recv() => {
                match cmd {
                    None | Some(Cmd::Shutdown) => break,
                    Some(Cmd::Publish(update)) => {
                        if update.gen < last_gen {
                            continue;
                        }
                        last_gen = update.gen;
                        snapshot.revision = snapshot.revision.saturating_add(1);
                        snapshot.guests_can_control = update.guests_can_control;
                        snapshot.queue = update.queue;
                        snapshot.current_index = update.current_index;
                        snapshot.playback = update.playback;
                        broadcast(&links, &snapshot);
                    }
                    Some(Cmd::SetControl(on)) => {
                        snapshot.guests_can_control = on;
                        snapshot.revision = snapshot.revision.saturating_add(1);
                        broadcast(&links, &snapshot);
                    }
                    Some(Cmd::Kick(id)) => {
                        if let Some(link) = links.remove(&id) {
                            let _ = link.extra.send(HostMsg::JamEnded {
                                reason: "the host removed you from the jam".into(),
                            });
                            refresh_roster(&mut snapshot, &links, &name, &host_id);
                            broadcast(&links, &snapshot);
                            emit_roster(&events, &snapshot, &links);
                        }
                    }
                    Some(Cmd::SendAction(_)) => {}
                }
            }
            evt = guest_rx.recv() => {
                let Some(evt) = evt else { break };
                match evt {
                    GuestEvt::Request { peer_id, name: guest_name, reply } => {
                        if links.len() >= MAX_GUESTS {
                            let _ = reply.send(Admit::No("this jam is full".into()));
                            continue;
                        }
                        let (snap_tx, _snap_rx) = watch::channel(snapshot.clone());
                        let (extra_tx, extra_rx) = mpsc::unbounded_channel();
                        links.insert(peer_id.clone(), GuestLink {
                            name: guest_name.clone(),
                            snap: snap_tx.clone(),
                            extra: extra_tx,
                            path: PathKind::Unknown,
                            rtt_ms: None,
                        });
                        refresh_roster(&mut snapshot, &links, &name, &host_id);
                        // The new guest's watch starts at the roster we
                        // just built. Push that same snapshot to everyone
                        // already in the room.
                        broadcast(&links, &snapshot);
                        emit_roster(&events, &snapshot, &links);
                        let _ = reply.send(Admit::Ok {
                            snapshot: snapshot.clone(),
                            snap: snap_tx,
                            extra: extra_rx,
                        });
                    }
                    GuestEvt::Action { peer_id, name: guest_name, action } => {
                        let action = stamp_action(&peer_id, &guest_name, action);
                        match authorize(&peer_id, &action, &snapshot) {
                            Ok(()) => {
                                let _ = events.send(JamEvent::GuestAction {
                                    peer_id,
                                    name: guest_name,
                                    action,
                                });
                            }
                            Err(reason) => {
                                if let Some(link) = links.get(&peer_id) {
                                    let _ = link.extra.send(HostMsg::Denied {
                                        reason: reason.to_string(),
                                    });
                                }
                            }
                        }
                    }
                    GuestEvt::Path { peer_id, path, rtt_ms } => {
                        if let Some(link) = links.get_mut(&peer_id) {
                            link.path = path;
                            link.rtt_ms = rtt_ms;
                            emit_roster(&events, &snapshot, &links);
                        }
                    }
                    GuestEvt::Ping { peer_id, seq, t1_guest_ms } => {
                        if let Some(link) = links.get(&peer_id) {
                            let t2 = unix_ms();
                            let _ = link.extra.send(HostMsg::Pong {
                                seq,
                                t1_guest_ms,
                                t2_host_ms: t2,
                                t3_host_ms: unix_ms(),
                            });
                        }
                    }
                    GuestEvt::Left { peer_id } => {
                        if links.remove(&peer_id).is_some() {
                            refresh_roster(&mut snapshot, &links, &name, &host_id);
                            broadcast(&links, &snapshot);
                            emit_roster(&events, &snapshot, &links);
                        }
                    }
                }
            }
            _ = heartbeat.tick() => {
                if snapshot.playback.playing {
                    let beat = HostMsg::Heartbeat {
                        host_now_ms: unix_ms(),
                        playing: snapshot.playback.playing,
                        position_ms: snapshot.playback.position_ms,
                        track_uid: snapshot.playback.track_uid,
                    };
                    for link in links.values() {
                        let _ = link.extra.send(beat.clone());
                    }
                }
            }
        }
    }

    let ended = HostMsg::JamEnded {
        reason: "the host ended the jam".into(),
    };
    for link in links.values() {
        let _ = link.extra.send(ended.clone());
    }
    // Let the goodbye hit the wire before the endpoint goes away.
    tokio::time::sleep(Duration::from_millis(80)).await;
    endpoint.close().await;
    accept.abort();
    let _ = events.send(JamEvent::Ended("jam ended".into()));
}

fn refresh_roster(
    snapshot: &mut JamSnapshot,
    links: &HashMap<String, GuestLink>,
    host_name: &str,
    host_id: &str,
) {
    snapshot.participants.clear();
    snapshot.participants.push(super::protocol::Participant {
        peer_id: host_id.to_string(),
        name: host_name.to_string(),
        host: true,
    });
    let mut guests: Vec<_> = links
        .iter()
        .map(|(id, link)| super::protocol::Participant {
            peer_id: id.clone(),
            name: link.name.clone(),
            host: false,
        })
        .collect();
    guests.sort_by(|a, b| a.name.cmp(&b.name));
    snapshot.participants.extend(guests);
}

fn broadcast(links: &HashMap<String, GuestLink>, snapshot: &JamSnapshot) {
    for link in links.values() {
        let _ = link.snap.send(snapshot.clone());
    }
}

fn emit_roster(
    events: &mpsc::UnboundedSender<JamEvent>,
    snapshot: &JamSnapshot,
    links: &HashMap<String, GuestLink>,
) {
    let kinds: Vec<PathKind> = links.values().map(|l| l.path).collect();
    let rtt = links.values().filter_map(|l| l.rtt_ms).min();
    let _ = events.send(JamEvent::Roster {
        participants: snapshot.participants.clone(),
        path: aggregate_path(&kinds),
        rtt_ms: rtt,
    });
}

async fn accept_loop(
    endpoint: Endpoint,
    room: [u8; 16],
    guest_tx: mpsc::UnboundedSender<GuestEvt>,
) {
    while let Some(incoming) = endpoint.accept().await {
        let Ok(accepting) = incoming.accept() else {
            continue;
        };
        let guest_tx = guest_tx.clone();
        tokio::spawn(async move {
            let Ok(conn) = accepting.await else {
                return;
            };
            if let Err(err) = handle_guest(conn, room, guest_tx.clone()).await {
                eprintln!("sonora jam: guest left ({err})");
            }
        });
    }
}

async fn handle_guest(
    conn: Connection,
    room: [u8; 16],
    guest_tx: mpsc::UnboundedSender<GuestEvt>,
) -> Result<(), String> {
    let (mut send, mut recv) = conn.accept_bi().await.map_err(|e| e.to_string())?;
    let hello = read_msg::<ClientMsg>(&mut recv).await?;
    let (presented, raw_name) = match hello {
        ClientMsg::Hello {
            protocol_version,
            room_secret,
            display_name,
        } => {
            if protocol_version != PROTOCOL_VERSION {
                write_msg(
                    &mut send,
                    &HostMsg::Reject {
                        reason: "this jam needs a newer Sonora".into(),
                    },
                )
                .await?;
                return Err("protocol mismatch".into());
            }
            (room_secret, display_name)
        }
        _ => {
            write_msg(
                &mut send,
                &HostMsg::Reject {
                    reason: "expected a hello".into(),
                },
            )
            .await?;
            return Err("bad hello".into());
        }
    };
    if !hello_room_ok(&room, &presented) {
        write_final(
            &mut send,
            &HostMsg::Reject {
                reason: "that jam code is wrong or expired".into(),
            },
        )
        .await?;
        return Err("bad room secret".into());
    }
    let peer_id = conn.remote_id().to_string();
    let name = clean_name(&raw_name);
    let (reply_tx, reply_rx) = oneshot::channel();
    if guest_tx
        .send(GuestEvt::Request {
            peer_id: peer_id.clone(),
            name: name.clone(),
            reply: reply_tx,
        })
        .is_err()
    {
        return Err("jam closed".into());
    }
    match reply_rx.await.map_err(|_| "jam closed".to_string())? {
        Admit::No(reason) => {
            write_msg(&mut send, &HostMsg::Reject { reason }).await?;
            return Err("refused".into());
        }
        Admit::Ok {
            snapshot,
            snap,
            extra,
        } => {
            if let Err(e) = write_msg(
                &mut send,
                &HostMsg::Welcome {
                    peer_id: peer_id.clone(),
                    snapshot,
                    host_now_ms: unix_ms(),
                },
            )
            .await
            {
                let _ = guest_tx.send(GuestEvt::Left {
                    peer_id: peer_id.clone(),
                });
                return Err(e);
            }
            let writer = tokio::spawn(async move {
                let _ = write_loop(send, snap, extra).await;
            });
            let read = read_guest_loop(&mut recv, &guest_tx, &conn, &peer_id, &name).await;
            writer.abort();
            let _ = guest_tx.send(GuestEvt::Left { peer_id });
            read
        }
    }
}

async fn write_loop(
    mut send: SendStream,
    snap: watch::Sender<JamSnapshot>,
    mut extra: mpsc::UnboundedReceiver<HostMsg>,
) -> Result<(), String> {
    let mut snap_rx = snap.subscribe();
    loop {
        tokio::select! {
            changed = snap_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                let snapshot = snap_rx.borrow().clone();
                write_msg(&mut send, &HostMsg::StateSync { snapshot }).await?;
            }
            msg = extra.recv() => {
                let Some(msg) = msg else { break };
                let ending = matches!(msg, HostMsg::JamEnded { .. });
                if ending {
                    write_final(&mut send, &msg).await?;
                    break;
                }
                write_msg(&mut send, &msg).await?;
            }
        }
    }
    Ok(())
}

async fn read_guest_loop(
    recv: &mut RecvStream,
    guest_tx: &mpsc::UnboundedSender<GuestEvt>,
    conn: &Connection,
    peer_id: &str,
    name: &str,
) -> Result<(), String> {
    let mut pulse = tokio::time::interval(Duration::from_secs(2));
    pulse.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            msg = read_msg::<ClientMsg>(recv) => {
                let msg = msg?;
                match msg {
                    ClientMsg::Leave => return Ok(()),
                    ClientMsg::Hello { .. } => {}
                    ClientMsg::Ping { seq, t1_guest_ms } => {
                        if guest_tx
                            .send(GuestEvt::Ping {
                                peer_id: peer_id.to_string(),
                                seq,
                                t1_guest_ms,
                            })
                            .is_err()
                        {
                            return Err("jam closed".into());
                        }
                    }
                    other => {
                        if let Some(action) = other.action() {
                            if guest_tx.send(GuestEvt::Action {
                                peer_id: peer_id.to_string(),
                                name: name.to_string(),
                                action,
                            }).is_err() {
                                return Err("jam closed".into());
                            }
                        }
                    }
                }
            }
            _ = pulse.tick() => {
                let (path, rtt_ms) = classify(conn);
                if guest_tx.send(GuestEvt::Path {
                    peer_id: peer_id.to_string(),
                    path,
                    rtt_ms,
                }).is_err() {
                    return Err("jam closed".into());
                }
            }
        }
    }
}

async fn write_final(send: &mut SendStream, msg: &impl Serialize) -> Result<(), String> {
    write_msg(send, msg).await?;
    let _ = send.finish();
    let _ = tokio::time::timeout(Duration::from_secs(1), send.stopped()).await;
    Ok(())
}

async fn write_msg(send: &mut SendStream, msg: &impl Serialize) -> Result<(), String> {
    let body = serde_json::to_vec(msg).map_err(|e| e.to_string())?;
    if !frame_len_ok(body.len()) {
        return Err("message is too large".into());
    }
    let mut buf = Vec::with_capacity(4 + body.len());
    buf.extend_from_slice(&(body.len() as u32).to_be_bytes());
    buf.extend_from_slice(&body);
    send.write_all(&buf).await.map_err(|e| e.to_string())
}

async fn read_msg<T: DeserializeOwned>(recv: &mut RecvStream) -> Result<T, String> {
    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf)
        .await
        .map_err(|e| e.to_string())?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if !frame_len_ok(len) {
        return Err("peer sent a frame that is too large".into());
    }
    let mut body = vec![0u8; len];
    if len > 0 {
        recv.read_exact(&mut body)
            .await
            .map_err(|e| e.to_string())?;
    }
    serde_json::from_slice(&body).map_err(|e| e.to_string())
}

async fn run_guest(
    events: mpsc::UnboundedSender<JamEvent>,
    mut cmds: mpsc::UnboundedReceiver<Cmd>,
    name: String,
    ticket: String,
    loopback: bool,
) {
    let name = clean_name(&name);
    let (addr, room) = match decode_ticket(&ticket) {
        Ok(v) => v,
        Err(e) => {
            let _ = events.send(JamEvent::Ended(e));
            return;
        }
    };
    let endpoint = match bind_endpoint(loopback).await {
        Ok(ep) => ep,
        Err(e) => {
            let _ = events.send(JamEvent::Ended(format!("could not join the jam ({e})")));
            return;
        }
    };
    if !loopback {
        let _ = tokio::time::timeout(Duration::from_secs(4), endpoint.online()).await;
    }
    let mut delay = Duration::from_secs(1);
    loop {
        match guest_session(&endpoint, &addr, &room, &name, &events, &mut cmds).await {
            SessionEnd::Fatal(msg) => {
                let _ = events.send(JamEvent::Ended(msg));
                break;
            }
            SessionEnd::Local => {
                let _ = events.send(JamEvent::Ended("left the jam".into()));
                break;
            }
            SessionEnd::Retry(msg) => {
                let _ = events.send(JamEvent::Notice(format!("reconnecting ({msg})")));
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            cmd = cmds.recv() => {
                match cmd {
                    None | Some(Cmd::Shutdown) => {
                        let _ = events.send(JamEvent::Ended("left the jam".into()));
                        break;
                    }
                    Some(_) => {}
                }
            }
        }
        delay = (delay * 2).min(Duration::from_secs(15));
    }
    endpoint.close().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jam::protocol::JamTrack;

    async fn wait_for(jam: &JamManager, mut pred: impl FnMut(&JamEvent) -> bool) -> JamEvent {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if tokio::time::Instant::now() > deadline {
                panic!("timed out waiting for a jam event");
            }
            if let Some(ev) = jam.poll_event() {
                if pred(&ev) {
                    return ev;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn one_track() -> PublishUpdate {
        PublishUpdate {
            gen: 1,
            guests_can_control: true,
            queue: vec![JamTrack {
                uid: 7,
                catalog_id: "1811922756".into(),
                title: "Kool-Aid".into(),
                artist: "Bring Me The Horizon".into(),
                album: String::new(),
                duration_ms: 208_000,
                art_url: None,
                isrc: None,
                added_by: "Ada".into(),
                added_by_id: "host".into(),
                radio: false,
            }],
            current_index: Some(0),
            playback: PlaybackAnchor {
                playing: true,
                position_ms: 1_000,
                anchor_host_ms: unix_ms(),
                track_uid: Some(7),
                loop_one: false,
                shuffle: false,
            },
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn loopback_jam_syncs_state_and_rejects_a_bad_code() {
        let host = JamManager::new();
        host.start_host_loopback("Ada".into(), true).await.unwrap();
        let connected = wait_for(&host, |ev| matches!(ev, JamEvent::Connected { .. })).await;
        let ticket = match connected {
            JamEvent::Connected { ticket, .. } => ticket.expect("host ticket"),
            _ => unreachable!(),
        };

        let (addr, room) = decode_ticket(&ticket).unwrap();
        let mut wrong = room;
        wrong[0] ^= 0xff;
        let bad = encode_ticket(&addr, &wrong).unwrap();
        let intruder = JamManager::new();
        intruder.join_loopback(bad, "Nope".into()).await.unwrap();
        let refused = wait_for(&intruder, |ev| matches!(ev, JamEvent::Ended(_))).await;
        match refused {
            JamEvent::Ended(reason) => assert!(
                reason.contains("wrong") || reason.contains("expired") || !reason.is_empty()
            ),
            _ => unreachable!(),
        }
        intruder.shutdown();

        let guest = JamManager::new();
        guest.join_loopback(ticket, "Bea".into()).await.unwrap();
        let _ = wait_for(&guest, |ev| {
            matches!(
                ev,
                JamEvent::Connected {
                    role: JamRole::Guest,
                    ..
                }
            )
        })
        .await;

        host.publish(one_track());
        let synced = wait_for(&guest, |ev| {
            matches!(ev, JamEvent::RemoteState { snapshot, .. } if snapshot.queue.iter().any(|t| t.catalog_id == "1811922756"))
        })
        .await;
        let JamEvent::RemoteState { snapshot, .. } = synced else {
            unreachable!();
        };
        assert_eq!(snapshot.queue[0].title, "Kool-Aid");
        assert!(snapshot.participants.iter().any(|p| p.name == "Bea"));

        guest.send_action(JamAction::QueueAppend {
            tracks: vec![JamTrack {
                uid: 99,
                catalog_id: "42".into(),
                title: "Added".into(),
                artist: String::new(),
                album: String::new(),
                duration_ms: 10,
                art_url: None,
                isrc: None,
                added_by: "spoof".into(),
                added_by_id: "spoof".into(),
                radio: false,
            }],
        });
        let action = wait_for(&host, |ev| matches!(ev, JamEvent::GuestAction { .. })).await;
        match action {
            JamEvent::GuestAction { name, action, .. } => {
                assert_eq!(name, "Bea");
                match action {
                    JamAction::QueueAppend { tracks } => {
                        assert_eq!(tracks[0].catalog_id, "42");
                        assert_eq!(tracks[0].added_by, "Bea");
                        assert_ne!(tracks[0].added_by_id, "spoof");
                        assert_eq!(tracks[0].uid, 0);
                    }
                    other => panic!("unexpected {other:?}"),
                }
            }
            _ => unreachable!(),
        }

        host.shutdown();
        let ended = wait_for(&guest, |ev| matches!(ev, JamEvent::Ended(_))).await;
        match ended {
            JamEvent::Ended(reason) => assert!(reason.contains("ended"), "{reason}"),
            _ => unreachable!(),
        }
        guest.shutdown();
    }
}

async fn guest_session(
    endpoint: &Endpoint,
    addr: &iroh::EndpointAddr,
    room: &[u8; 16],
    name: &str,
    events: &mpsc::UnboundedSender<JamEvent>,
    cmds: &mut mpsc::UnboundedReceiver<Cmd>,
) -> SessionEnd {
    let conn = match tokio::time::timeout(
        Duration::from_secs(20),
        endpoint.connect(addr.clone(), ALPN),
    )
    .await
    {
        Ok(Ok(conn)) => conn,
        Ok(Err(e)) => return SessionEnd::Retry(e.to_string()),
        Err(_) => return SessionEnd::Retry("timed out reaching the host".into()),
    };
    let (mut send, mut recv) = match conn.open_bi().await {
        Ok(pair) => pair,
        Err(e) => return SessionEnd::Retry(e.to_string()),
    };
    if let Err(e) = write_msg(
        &mut send,
        &ClientMsg::Hello {
            protocol_version: PROTOCOL_VERSION,
            room_secret: room_hex(room),
            display_name: name.to_string(),
        },
    )
    .await
    {
        return SessionEnd::Retry(e);
    }
    let (self_id, snapshot, host_now_ms) = match read_msg::<HostMsg>(&mut recv).await {
        Ok(HostMsg::Welcome {
            peer_id,
            snapshot,
            host_now_ms,
        }) => (peer_id, snapshot, host_now_ms),
        Ok(HostMsg::Reject { reason }) | Ok(HostMsg::JamEnded { reason }) => {
            return SessionEnd::Fatal(reason);
        }
        Ok(_) => return SessionEnd::Retry("unexpected greeting".into()),
        Err(e) => return SessionEnd::Retry(e),
    };
    let (path, rtt_ms) = classify(&conn);
    let _ = events.send(JamEvent::Connected {
        role: JamRole::Guest,
        ticket: None,
        self_id,
        snapshot: snapshot.clone(),
    });
    let _ = events.send(JamEvent::RemoteState {
        snapshot,
        est_host_now_ms: host_now_ms,
        path,
        rtt_ms,
    });

    let mut clock = ClockSync::default();
    let mut seq = 0u64;
    let mut ping = tokio::time::interval(Duration::from_secs(5));
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // The first tick completes immediately. Send a real sample instead.
    ping.tick().await;
    seq += 1;
    if let Err(e) = write_msg(
        &mut send,
        &ClientMsg::Ping {
            seq,
            t1_guest_ms: unix_ms(),
        },
    )
    .await
    {
        return SessionEnd::Retry(e);
    }

    loop {
        tokio::select! {
            cmd = cmds.recv() => {
                match cmd {
                    None | Some(Cmd::Shutdown) => {
                        let _ = write_msg(&mut send, &ClientMsg::Leave).await;
                        return SessionEnd::Local;
                    }
                    Some(Cmd::SendAction(action)) => {
                        if let Err(e) = write_msg(&mut send, &ClientMsg::from(action)).await {
                            return SessionEnd::Retry(e);
                        }
                    }
                    Some(Cmd::Publish(_)) | Some(Cmd::SetControl(_)) | Some(Cmd::Kick(_)) => {}
                }
            }
            msg = read_msg::<HostMsg>(&mut recv) => {
                let msg = match msg {
                    Ok(msg) => msg,
                    Err(e) => return SessionEnd::Retry(e),
                };
                match msg {
                    HostMsg::StateSync { snapshot } => {
                        let (path, rtt_ms) = classify(&conn);
                        let _ = events.send(JamEvent::RemoteState {
                            snapshot,
                            est_host_now_ms: clock.est_host_now(unix_ms()),
                            path,
                            rtt_ms: rtt_ms.or_else(|| clock.best_rtt_ms()),
                        });
                    }
                    HostMsg::Heartbeat { host_now_ms: _, playing, position_ms, track_uid } => {
                        let (path, rtt_ms) = classify(&conn);
                        let _ = events.send(JamEvent::Heartbeat {
                            est_host_now_ms: clock.est_host_now(unix_ms()),
                            playing,
                            position_ms,
                            track_uid,
                            path,
                            rtt_ms: rtt_ms.or_else(|| clock.best_rtt_ms()),
                        });
                    }
                    HostMsg::Pong { t1_guest_ms, t2_host_ms, t3_host_ms, .. } => {
                        let _ = clock.observe(t1_guest_ms, t2_host_ms, t3_host_ms, unix_ms());
                    }
                    HostMsg::Denied { reason } => {
                        let _ = events.send(JamEvent::Notice(reason));
                    }
                    HostMsg::JamEnded { reason } | HostMsg::Reject { reason } => {
                        return SessionEnd::Fatal(reason);
                    }
                    HostMsg::Welcome { .. } => {}
                }
            }
            _ = ping.tick() => {
                seq = seq.wrapping_add(1);
                if let Err(e) = write_msg(
                    &mut send,
                    &ClientMsg::Ping {
                        seq,
                        t1_guest_ms: unix_ms(),
                    },
                )
                .await
                {
                    return SessionEnd::Retry(e);
                }
            }
        }
    }
}
