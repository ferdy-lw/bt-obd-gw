/// Attempt to build a guaranteed delivery request response for the ELM messages.
/// Using ack msgs and retries. Ultimately too complicated and still error prone
/// when HTTP already handles large packets and robust error conditions
/* Error messages

       #[error("ESPNow failed to find peer")]
       FailedToFindPeer,

       #[error("ESPNow channel disconnected")]
       ESPNowCBDisconnected,

       #[error("ESPNow failed to send")]
       ESPNowFailedSend,

       #[error("ESPNow no ACK")]
       ESPNowNoAck,

       #[error("ESPNow invalid message")]
       ESPNowInvalidMessage,

*/
use std::{
    sync::mpsc::{self, Receiver, SyncSender, TryRecvError},
    thread,
    time::{Duration, Instant},
};

use esp_idf_svc::espnow::{EspNow, PeerInfo, ReceiveInfo, SendStatus, BROADCAST};
use esp_idf_svc::sys::{ESP_NOW_ETH_ALEN, ESP_NOW_MAX_DATA_LEN};
use log::*;

use crate::error::{LedBlink, ReadObdError};
use anyhow::{Context, Result};

/*
#[repr(u8)]
enum MsgType {
    Ping = 0x01,
    Info = 0x02,
}

impl Into<[u8; MAX_DATA_LEN]> for MsgType {
    fn into(self) -> [u8] {
        match self {
            Ping => [0x01, 0xAA],
            Info => [0x02],
        }
    }
}
*/

pub const MAX_MAC_LEN: usize = ESP_NOW_ETH_ALEN as _;
pub const MAX_DATA_LEN: usize = ESP_NOW_MAX_DATA_LEN as _;

const BROADCAST_PING_INTERVAL: Duration = Duration::from_secs(2);
const ESP_NOW_INIT_TIMEOUT: Duration = Duration::from_secs(10);
const SEND_TRY: u8 = 6;
const ESP_NOW_SEND_CB_TIMEOUT: Duration = Duration::from_millis(200);
const ACK_WAIT_TIMEOUT: Duration = Duration::from_millis(200);

const MSG_TYPE_CTL: u8 = 0x01;

// | MsgType | Msg ID | Data... 248
// | MultiFrame | Msg ID | FramePos | Data... 247
#[repr(u8)]
pub enum MsgType {
    Ctl,
    BCast,
    Ready,
    Ack,
    Frame,
    MultiFrame,
}

impl MsgType {
    const ACK_MSG: u8 = MsgType::Ack as _;
    const FRAME_MSG: u8 = MsgType::Frame as _;
    const MULTIFRAME_MSG: u8 = MsgType::MultiFrame as _;
    pub const RQST_MSG: u8 = MsgType::Frame as _;
}

impl From<u8> for MsgType {
    fn from(value: u8) -> Self {
        match value {
            0 => MsgType::Ctl,
            1 => MsgType::BCast,
            2 => MsgType::Ready,
            3 => MsgType::Ack,
            4 => MsgType::Frame,
            5 => MsgType::MultiFrame,
            _ => panic!("Wrong msg type"),
        }
    }
}

const BROADCAST_MSG: [u8; 2] = [MSG_TYPE_CTL, 0xAA];
const READY_MSG: [u8; 2] = [MSG_TYPE_CTL, 0xBB];

type EspNowData = heapless::Vec<u8, MAX_DATA_LEN>;
type MacAddr = [u8; MAX_MAC_LEN];
pub type ChannelData = (MacAddr, EspNowData);

fn espnow_recv_cb(espnow_tx: SyncSender<ChannelData>) -> impl FnMut(&ReceiveInfo, &[u8]) {
    move |info, data| {
        debug!("espnow info {info:?}, data {data:?}");

        let data = EspNowData::from_slice(data).unwrap();

        espnow_tx.send((info.src_addr.to_owned(), data)).unwrap();
    }
}

fn espnow_send_cb(espnow_send_tx: SyncSender<bool>) -> impl FnMut(&[u8], SendStatus) {
    move |peer, status| {
        debug!(
            "espnow send to peer {}, status {:?}",
            pretty_mac(peer),
            status
        );

        let mut status_ok = true;
        if let SendStatus::FAIL = status {
            error!("Espnow failed to send to peer {}", pretty_mac(peer));
            status_ok = false;
        }

        if let Err(e) = espnow_send_tx.send(status_ok) {
            error!("Espnow failed to send status in channel: {e}")
        }
    }
}

pub struct Espnow {
    espnow: EspNow<'static>,
    espnow_rx: Receiver<ChannelData>,
    espnow_send_rx: Receiver<bool>,
    peer_addr: MacAddr,
    send_buf: EspNowData,
}

impl Espnow {
    pub fn new(espnow: EspNow<'static>) -> Result<Self> {
        let (espnow_tx, espnow_rx) = mpsc::sync_channel(5);
        let (espnow_send_tx, espnow_send_rx) = mpsc::sync_channel(5);

        espnow
            .register_recv_cb(espnow_recv_cb(espnow_tx))
            .context("Failed to register ESPNOW recv callback")?;

        espnow
            .register_send_cb(espnow_send_cb(espnow_send_tx))
            .context("Failed to register ESPNOW send callback")?;

        Ok(Self {
            espnow,
            espnow_rx,
            espnow_send_rx,
            peer_addr: BROADCAST,
            send_buf: EspNowData::new(),
        })
    }

    pub fn connect_peer(&mut self, led_blink: &SyncSender<LedBlink>) -> Result<()> {
        self.espnow.add_peer(Espnow::get_peer_info(BROADCAST))?;

        let start = Instant::now();
        let mut last_broadcast: Option<Instant> = None;

        loop {
            thread::sleep(Duration::from_millis(10));

            if last_broadcast.is_none_or(|t| t.elapsed() > BROADCAST_PING_INTERVAL) {
                led_blink.send(LedBlink::Times(1))?;

                self.espnow.send(BROADCAST, &BROADCAST_MSG)?;

                last_broadcast = Some(Instant::now());
            }

            match self.espnow_rx.try_recv() {
                Ok((peer, data)) => {
                    if data == BROADCAST_MSG {
                        self.peer_addr = peer;
                        self.espnow.del_peer(BROADCAST)?;
                        self.espnow.add_peer(Espnow::get_peer_info(peer))?;
                        info!("Found and Added peer {}", pretty_mac(&peer));
                        break;
                    }
                    error!(
                        "Unknown broadcast response from {}: {data:?}",
                        pretty_mac(&peer)
                    );
                }
                Err(TryRecvError::Disconnected) => {
                    Err(ReadObdError::ESPNowCBDisconnected)
                        .context("EspNow find peer disconnected")?;
                }
                _ => {} // No messages in channel
            }

            if start.elapsed() > ESP_NOW_INIT_TIMEOUT {
                Err(ReadObdError::FailedToFindPeer)?;
            }
        }

        Ok(())
    }

    pub fn notify_peer(&self) -> Result<()> {
        self.espnow.send(self.peer_addr, &READY_MSG)?;

        Ok(())
    }

    pub fn get_request(&self) -> Result<Option<EspNowData>> {
        match self.espnow_rx.try_recv() {
            Ok((_, data)) => Ok(Some(data)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => {
                Err(ReadObdError::ESPNowCBDisconnected).context("EspNow request disconnected")
            }
        }
    }

    pub fn send_data(&mut self, msg_id: u8, data: &[u8]) -> Result<Option<EspNowData>> {
        self.send_buf.clear();

        let mut idx = 0;

        // Multiframe, the frames are special prefixed, with the final frame being the same as
        // the single frame marker
        while data.len() - idx + 1 > MAX_DATA_LEN {
            self.send_buf.push(MsgType::MULTIFRAME_MSG).unwrap();
            self.send_buf.push(msg_id).unwrap();
            self.send_buf
                .extend_from_slice(&data[idx..(idx + MAX_DATA_LEN - 2)])
                .unwrap();

            self.send_msg(&msg_id)?; // We don't expect any new request at this point
                                     // self.espnow.send(self.peer_addr, &self.send_buf)?;
            self.send_buf.clear();

            idx += MAX_DATA_LEN - 2;

            thread::sleep(Duration::from_millis(10));
        }

        if data.len() - idx > 0 {
            self.send_buf.push(MsgType::FRAME_MSG).unwrap();
            self.send_buf.push(msg_id).unwrap();
            self.send_buf.extend_from_slice(&data[idx..]).unwrap();

            self.send_msg(&msg_id)
            // self.espnow.send(self.peer_addr, &self.send_buf)?;
        } else {
            Ok(None)
        }
    }

    fn get_peer_info(peer_addr: MacAddr) -> PeerInfo {
        PeerInfo {
            peer_addr,
            channel: 1,
            ifidx: esp_idf_svc::sys::wifi_interface_t_WIFI_IF_STA,
            encrypt: false,
            ..Default::default()
        }
    }

    fn send_ack(&mut self) -> Result<()> {
        Ok(())
    }

    fn send_msg(&mut self, cur_msg_id: &u8) -> Result<Option<EspNowData>> {
        let mut try_count = SEND_TRY;

        //send ack
        loop {
            'send: while try_count > 0 {
                try_count -= 1;

                if let Err(e) = self.espnow.send(self.peer_addr, &self.send_buf) {
                    error!("EspNow send failed {e}");
                    thread::sleep(Duration::from_millis(20));
                } else {
                    let start = Instant::now();
                    loop {
                        if start.elapsed() > ESP_NOW_SEND_CB_TIMEOUT {
                            Err(ReadObdError::ESPNowFailedSend)
                                .context("send CB did not respond")?;
                        }

                        match self.espnow_send_rx.try_recv() {
                            Ok(true) => break 'send,
                            Ok(false) => break,
                            Err(TryRecvError::Empty) => thread::sleep(Duration::from_millis(10)),
                            Err(TryRecvError::Disconnected) => {
                                Err(ReadObdError::ESPNowCBDisconnected).context("send CB")?
                            }
                        };
                    }
                }
            }

            if try_count == 0 {
                Err(ReadObdError::ESPNowFailedSend)?
            }

            // MultiFrame ack on each frame, it wont send a req until the last frame because the
            // receiver will be waiting for the last frame, and acking before moving to the next request

            // Can only get REQ or ACK.
            // In loop
            //   an ACK for our current req/rsp cycle -> good, go to wait for next req:   None
            //   an REQ for 'next' cycle -> good, we missed ack so use req and dont wait:  Some(req)
            //   any other msg <= current cycle -> dump and get next msg
            //   no msg within ACK timeout, send again
            //   no msg within 5 tries, go to wait for next req:  None

            let start = Instant::now();
            loop {
                if start.elapsed() > ACK_WAIT_TIMEOUT {
                    break;
                }

                match self.espnow_rx.try_recv() {
                    Ok((_peer, data)) => {
                        if let Some(msg_id) = data.get(1) {
                            if Espnow::is_msg_id_old(cur_msg_id, msg_id) {
                                error!(
                                "Old message ({:?}) current id ({cur_msg_id}) found, discarding",
                                data
                            );
                            } else {
                                // msg id is same or newer
                                match data.first() {
                                    Some(&MsgType::ACK_MSG) => return Ok(None), // assuming same msg_id
                                    Some(&MsgType::RQST_MSG) => return Ok(Some(data)), // Could get same (id) msg again?
                                    Some(_) => {
                                        error!(
                                        "Unexpected message, ignoring and waiting for ack ({:?})",
                                        data
                                    )
                                    }
                                    None => Err(ReadObdError::ESPNowInvalidMessage)
                                        .context("rcv no msg type, too short")?,
                                }
                            }
                        } else {
                            Err(ReadObdError::ESPNowInvalidMessage)
                                .context("rcv no msg id, too short")?
                        }
                    }
                    Err(TryRecvError::Empty) => thread::sleep(Duration::from_millis(10)),
                    Err(TryRecvError::Disconnected) => {
                        Err(ReadObdError::ESPNowCBDisconnected).context("recv CB")?
                    }
                }
            }
        }
    }

    /// returns true if the msg_id is before the cur_msg_id, i.e. it's old
    fn is_msg_id_old(cur_msg_id: &u8, msg_id: &u8) -> bool {
        (*cur_msg_id == 0 && *msg_id > 0) || (*msg_id < *cur_msg_id)
    }
}

fn pretty_mac(mac: &[u8]) -> String {
    let mut s = String::new();

    for (i, byte) in mac.iter().enumerate() {
        s.push_str(&format!("{:02X}", byte));
        if i < mac.len() - 1 {
            s.push(':');
        }
    }

    s
}
