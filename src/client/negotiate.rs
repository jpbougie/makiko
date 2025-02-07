use bytes::Bytes;
use rand::Rng as _;
use std::cmp::max;
use std::future::Future as _;
use std::pin::Pin;
use std::task::Context;
use std::time::Instant;
use tokio::sync::oneshot;
use crate::error::{Error, Result, AlgoNegotiateError};
use crate::cipher::{CipherAlgo, CipherAlgoVariant, PacketEncrypt, PacketDecrypt};
use crate::codec::{PacketEncode, PacketDecode};
use crate::codes::msg;
use crate::kex::{Kex, KexAlgo, KexInput, KexOutput};
use crate::mac::{self, MacAlgo, MacAlgoVariant};
use crate::pubkey::{PubkeyAlgo, Pubkey, SignatureVerified};
use super::{auth, ext};
use super::client_event::{ClientEvent, AcceptPubkey, PubkeyAccepted};
use super::client_state::{self, ClientState};
use super::pump::Pump;
use super::recv::ResultRecvState;

#[derive(Default)]
pub(super) struct NegotiateState {
    state: State,
    our_kex_init: Option<OurKexInit>,
    their_kex_init: Option<TheirKexInit>,
    algos: Option<Algos>,
    kex: Option<Box<dyn Kex + Send>>,
    kex_output: Option<KexOutput>,
    signature_verified: Option<SignatureVerified>,
    pubkey_event: Option<ClientEvent>,
    accepted_rx: Option<oneshot::Receiver<Result<PubkeyAccepted>>>,
    pubkey_accepted: Option<PubkeyAccepted>,
    new_keys_sent: bool,
    new_keys_recvd: bool,
    done_txs: Vec<oneshot::Sender<Result<()>>>,
}

#[derive(Debug, Copy, Clone)]
enum State {
    Idle,
    KexInit,
    Kex,
    AcceptPubkey,
    NewKeys,
    Done,
}

struct OurKexInit {
    payload: Bytes,
    kex_algos: Vec<&'static KexAlgo>,
    server_pubkey_algos: Vec<&'static PubkeyAlgo>,
    cipher_algos_cts: Vec<&'static CipherAlgo>,
    cipher_algos_stc: Vec<&'static CipherAlgo>,
    mac_algos_cts: Vec<&'static MacAlgo>,
    mac_algos_stc: Vec<&'static MacAlgo>,
    packet_seq: u32,
}

#[derive(Debug)]
struct TheirKexInit {
    payload: Bytes,
    kex_algos: Vec<String>,
    server_pubkey_algos: Vec<String>,
    cipher_algos_cts: Vec<String>,
    cipher_algos_stc: Vec<String>,
    mac_algos_cts: Vec<String>,
    mac_algos_stc: Vec<String>,
}

struct Algos {
    kex: &'static KexAlgo,
    server_pubkey: &'static PubkeyAlgo,
    cipher_cts: &'static CipherAlgo,
    cipher_stc: &'static CipherAlgo,
    mac_cts: &'static MacAlgo,
    mac_stc: &'static MacAlgo,
}

#[derive(Debug)]
pub(super) struct LastKex {
    done: bool,
    recvd_bytes: u64,
    sent_bytes: u64,
    instant: Instant,
}

pub(super) fn init_negotiate() -> NegotiateState {
    NegotiateState { state: State::KexInit, .. NegotiateState::default() }
}

impl Default for State {
    fn default() -> Self { State::Idle }
}

pub(super) fn init_last_kex() -> LastKex {
    LastKex {
        done: false,
        recvd_bytes: 0,
        sent_bytes: 0,
        instant: Instant::now(),
    }
}

pub(super) fn pump_negotiate(st: &mut ClientState, cx: &mut Context) -> Result<Pump> {
    match st.negotiate_st.state {
        State::Idle => {
            if auth::is_authenticated(st) {
                let recvd_after_kex = st.codec.recv_pipe.recvd_bytes() - st.last_kex.recvd_bytes;
                let sent_after_kex = st.codec.send_pipe.sent_bytes() - st.last_kex.sent_bytes;
                let duration_after_kex = Instant::now() - st.last_kex.instant;
                if max(recvd_after_kex, sent_after_kex) > st.config.rekey_after_bytes ||
                    duration_after_kex > st.config.rekey_after_duration
                {
                    start_kex(st, None);
                    return Ok(Pump::Progress)
                }
            }
        },
        State::KexInit => {
            if st.negotiate_st.our_kex_init.is_none() {
                st.negotiate_st.our_kex_init = Some(send_kex_init(st));
            }

            if st.negotiate_st.our_kex_init.is_some() && st.negotiate_st.their_kex_init.is_some() {
                st.negotiate_st.algos = Some(negotiate_algos(st)?);
                let kex_algo = st.negotiate_st.algos.as_ref().unwrap().kex;
                st.negotiate_st.kex = Some((kex_algo.make_kex)(&mut *st.rng)?);
                st.negotiate_st.state = State::Kex;
                return Ok(Pump::Progress)
            }
        },
        State::Kex => {
            if let Some(payload) = st.negotiate_st.kex.as_mut().unwrap().send_packet()? {
                st.codec.send_pipe.feed_packet(&payload);
                return Ok(Pump::Progress)
            }

            let kex_input = KexInput {
                client_ident: &st.our_ident,
                server_ident: st.their_ident.as_ref().unwrap(),
                client_kex_init: &st.negotiate_st.our_kex_init.as_ref().unwrap().payload,
                server_kex_init: &st.negotiate_st.their_kex_init.as_ref().unwrap().payload,
            };
            let kex_output = pump_ready!(st.negotiate_st.kex.as_mut().unwrap().poll(kex_input))?;
            log::debug!("finished kex");

            if st.session_id.is_none() {
                st.session_id = Some(kex_output.exchange_hash.clone());
            }

            let pubkey_algo = st.negotiate_st.algos.as_ref().unwrap().server_pubkey;
            let pubkey = Pubkey::decode(kex_output.server_pubkey.clone())?;
            log::debug!("server pubkey {}", pubkey);

            let signature_verified = (pubkey_algo.verify)(
                &pubkey, &kex_output.exchange_hash, kex_output.server_exchange_hash_sign.clone())?;
            st.negotiate_st.signature_verified = Some(signature_verified);
            st.negotiate_st.kex_output = Some(kex_output);

            let (accepted_tx, accepted_rx) = oneshot::channel();
            let accept_tx = AcceptPubkey { accepted_tx };
            st.negotiate_st.pubkey_event = Some(ClientEvent::ServerPubkey(pubkey, accept_tx));
            st.negotiate_st.accepted_rx = Some(accepted_rx);
            st.negotiate_st.state = State::AcceptPubkey;
            return Ok(Pump::Progress)
        },
        State::AcceptPubkey => {
            if st.negotiate_st.pubkey_event.is_some() {
                let reserve_res = pump_ready!(st.event_tx.poll_reserve(cx));
                let pubkey_event = st.negotiate_st.pubkey_event.take().unwrap();
                if reserve_res.is_ok() {
                    let _: Result<_, _> = st.event_tx.send_item(pubkey_event);
                }
            }

            let accepted = pump_ready!(Pin::new(st.negotiate_st.accepted_rx.as_mut().unwrap()).poll(cx))
                .map_err(|err| Error::PubkeyAccept(Box::new(err)))??;
            log::debug!("server pubkey was accepted");
            st.negotiate_st.pubkey_accepted = Some(accepted);
            st.negotiate_st.state = State::NewKeys;
            return Ok(Pump::Progress)
        },
        State::NewKeys => {
            assert!(st.negotiate_st.signature_verified.is_some());
            assert!(st.negotiate_st.pubkey_accepted.is_some());

            if !st.negotiate_st.new_keys_sent {
                send_new_keys(st);
                st.negotiate_st.new_keys_sent = true;
                maybe_send_ext_info(st)?;
                return Ok(Pump::Progress)
            }

            if st.negotiate_st.new_keys_sent && st.negotiate_st.new_keys_recvd {
                st.negotiate_st.state = State::Done;
                return Ok(Pump::Progress)
            }
        },
        State::Done => {
            for done_tx in st.negotiate_st.done_txs.drain(..) {
                let _: Result<_, _> = done_tx.send(Ok(()));
            }
            st.negotiate_st = Box::new(NegotiateState::default());
            st.last_kex = LastKex {
                done: true,
                recvd_bytes: st.codec.recv_pipe.recvd_bytes(),
                sent_bytes: st.codec.send_pipe.sent_bytes(),
                instant: Instant::now(),
            };
            return Ok(Pump::Progress)
        },
    }
    Ok(Pump::Pending)
}

pub(super) fn recv_negotiate_packet(
    st: &mut ClientState,
    msg_id: u8,
    payload: &mut PacketDecode,
) -> ResultRecvState {
    match msg_id {
        msg::KEXINIT => recv_kex_init(st, payload),
        msg::NEWKEYS => recv_new_keys(st, payload),
        _ => Err(Error::PacketNotImplemented(msg_id)),
    }
}

pub(super) fn recv_kex_packet(
    st: &mut ClientState,
    msg_id: u8,
    payload: &mut PacketDecode,
) -> ResultRecvState {
    if let Some(kex) = st.negotiate_st.kex.as_mut() {
        kex.recv_packet(msg_id, payload)?;
        Ok(None)
    } else {
        Err(Error::Protocol("received unexpected kex message"))
    }
}

fn send_kex_init(st: &mut ClientState) -> OurKexInit {
    let cookie: [u8; 16] = st.rng.gen();

    fn get_algo_names<A: NamedAlgo>(algos: &[&A]) -> Vec<&'static str> {
        algos.iter().map(|algo| algo.name()).collect()
    }

    // RFC 4253, section 7.1
    let mut payload = PacketEncode::new();
    payload.put_u8(msg::KEXINIT);
    payload.put_raw(&cookie);
    payload.put_name_list(&{
        let mut names = get_algo_names(&st.config.kex_algos);
        // RFC 8308
        names.push("ext-info-c");
        names
    });
    payload.put_name_list(&get_algo_names(&st.config.server_pubkey_algos));
    payload.put_name_list(&get_algo_names(&st.config.cipher_algos));
    payload.put_name_list(&get_algo_names(&st.config.cipher_algos));
    payload.put_name_list(&get_algo_names(&st.config.mac_algos));
    payload.put_name_list(&get_algo_names(&st.config.mac_algos));
    payload.put_name_list(&["none"]);
    payload.put_name_list(&["none"]);
    payload.put_name_list(&[]);
    payload.put_name_list(&[]);
    payload.put_bool(false);
    payload.put_u32(0);
    let payload = payload.finish();
    let packet_seq = st.codec.send_pipe.feed_packet(&payload);

    log::debug!("sending SSH_MSG_KEXINIT");

    OurKexInit {
        payload,
        kex_algos: st.config.kex_algos.clone(),
        server_pubkey_algos: st.config.server_pubkey_algos.clone(),
        cipher_algos_cts: st.config.cipher_algos.clone(),
        cipher_algos_stc: st.config.cipher_algos.clone(),
        mac_algos_cts: st.config.mac_algos.clone(),
        mac_algos_stc: st.config.mac_algos.clone(),
        packet_seq,
    }
}

fn recv_kex_init(st: &mut ClientState, payload: &mut PacketDecode) -> ResultRecvState {
    // RFC 4253, section 7.1
    payload.skip(16)?; // cookie
    let kex_algos = payload.get_name_list()?; // kex_algorithms
    let server_pubkey_algos = payload.get_name_list()?; // server_host_key_algorithms
    let cipher_algos_cts = payload.get_name_list()?; // encryption_algorithms_client_to_server
    let cipher_algos_stc = payload.get_name_list()?; // encryption_algorithms_server_to_client
    let mac_algos_cts = payload.get_name_list()?; // mac_algorithms_client_to_server
    let mac_algos_stc = payload.get_name_list()?; // mac_algorithms_server_to_client
    payload.get_name_list()?; // compression_algorithms_client_to_server
    payload.get_name_list()?; // compression_algorithms_server_to_client
    payload.get_name_list()?; // languages_client_to_server
    payload.get_name_list()?; // languages_server_to_client
    let first_kex_packet_follows = payload.get_bool()?; // first_kex_packet_follows
    payload.get_u32()?; // reserved

    if first_kex_packet_follows {
        return Err(Error::Protocol("received SSH_MSG_KEXINIT with first_kex_packet_follows set"))
    }

    let kex_init = TheirKexInit {
        payload: Bytes::copy_from_slice(payload.as_original_bytes()),
        kex_algos,
        server_pubkey_algos,
        cipher_algos_cts,
        cipher_algos_stc,
        mac_algos_cts,
        mac_algos_stc,
    };
    log::debug!("received SSH_MSG_KEXINIT: {:?}", kex_init);

    match st.negotiate_st.state {
        State::Idle | State::KexInit if st.negotiate_st.their_kex_init.is_none() => {
            st.negotiate_st.their_kex_init = Some(kex_init);
            st.negotiate_st.state = State::KexInit;
            Ok(None)
        },
        _ => Err(Error::Protocol("received SSH_MSG_KEXINIT during negotiation")),
    }
}

pub(super) fn recv_unimplemented(st: &mut ClientState, packet_seq: u32) -> Result<bool> {
    if let Some(our_kex_init) = st.negotiate_st.our_kex_init.as_ref() {
        if our_kex_init.packet_seq == packet_seq {
            if st.negotiate_st.their_kex_init.is_some() {
                return Err(Error::Protocol("peer rejected our SSH_MSG_KEX_INIT, \
                    but they sent their SSH_MSG_KEX_INIT"))
            }

            if !st.last_kex.done {
                return Err(Error::Protocol("peer rejected our first SSH_MSG_KEX_INIT"))
            }

            for done_tx in st.negotiate_st.done_txs.drain(..) {
                let _: Result<_, _> = done_tx.send(Err(Error::RekeyRejected));
            }
            st.negotiate_st = Box::new(NegotiateState::default());
            return Ok(true)
        }
    }
    Ok(false)
}

fn negotiate_algos(st: &ClientState) -> Result<Algos> {
    fn negotiate_algo<A: NamedAlgo>(
        our_algos: &[&'static A],
        their_algos: &[String],
        name: &'static str,
    ) -> Result<&'static A> {
        for our_algo in our_algos.iter() {
            for their_algo in their_algos.iter() {
                if our_algo.name() == their_algo.as_str() {
                    log::debug!("negotiated algo {:?} for {}", their_algo, name);
                    return Ok(our_algo)
                }
            }
        }

        Err(Error::AlgoNegotiate(AlgoNegotiateError {
            algo_name: name.into(),
            our_algos: our_algos.iter().map(|a| a.name().into()).collect(),
            their_algos: their_algos.into(),
        }))
    }

    fn negotiate_mac_algo(
        cipher_algo: &CipherAlgo,
        our_algos: &[&'static MacAlgo],
        their_algos: &[String],
        name: &'static str,
    ) -> Result<&'static MacAlgo> {
        if cipher_algo.variant.is_aead() {
            Ok(&mac::INVALID)
        } else {
            negotiate_algo(our_algos, their_algos, name)
        }
    }

    let our = st.negotiate_st.our_kex_init.as_ref().unwrap();
    let their = st.negotiate_st.their_kex_init.as_ref().unwrap();

    let kex = negotiate_algo(&our.kex_algos, &their.kex_algos, "key exchange")?;
    let server_pubkey = negotiate_algo(
        &our.server_pubkey_algos, &their.server_pubkey_algos, "server public key")?;

    let cipher_cts = negotiate_algo(
        &our.cipher_algos_cts, &their.cipher_algos_cts, "cipher client-to-server")?;
    let cipher_stc = negotiate_algo(
        &our.cipher_algos_stc, &their.cipher_algos_stc, "cipher server-to-client")?;

    let mac_cts = negotiate_mac_algo(
        cipher_cts, &our.mac_algos_cts, &their.mac_algos_cts, "mac client-to-server")?;
    let mac_stc = negotiate_mac_algo(
        cipher_stc, &our.mac_algos_stc, &their.mac_algos_stc, "mac server-to-client")?;

    Ok(Algos { kex, server_pubkey, cipher_cts, cipher_stc, mac_cts, mac_stc })
}

trait NamedAlgo { fn name(&self) -> &'static str; }
impl NamedAlgo for KexAlgo { fn name(&self) -> &'static str { self.name } }
impl NamedAlgo for CipherAlgo { fn name(&self) -> &'static str { self.name } }
impl NamedAlgo for MacAlgo { fn name(&self) -> &'static str { self.name } }
impl NamedAlgo for PubkeyAlgo { fn name(&self) -> &'static str { self.name } }

fn recv_new_keys(st: &mut ClientState, _payload: &mut PacketDecode) -> ResultRecvState {
    match st.negotiate_st.state {
        State::Kex | State::AcceptPubkey | State::NewKeys => {
            if st.negotiate_st.new_keys_recvd {
                return Err(Error::Protocol("received SSH_MSG_NEWKEYS twice"))
            }
        },
        _ => return Err(Error::Protocol("received unexpected SSH_MSG_NEWKEYS")),
    }

    let algos = st.negotiate_st.algos.as_ref().unwrap();

    let cipher_algo = algos.cipher_stc;
    let cipher_key = derive_key(st, b'D', cipher_algo.key_len);
    let cipher_iv = derive_key(st, b'B', cipher_algo.iv_len);

    let (packet_decrypt, tag_len) = match cipher_algo.variant {
        CipherAlgoVariant::Standard(ref standard_algo) => {
            let decrypt = (standard_algo.make_decrypt)(&cipher_key, &cipher_iv);

            let mac_algo = algos.mac_stc;
            let mac_key = derive_key(st, b'F', mac_algo.key_len);
            let mac = (mac_algo.make_mac)(&mac_key);

            let packet_decrypt = match mac_algo.variant {
                MacAlgoVariant::EncryptAndMac => PacketDecrypt::EncryptAndMac(decrypt, mac),
                MacAlgoVariant::EncryptThenMac => PacketDecrypt::EncryptThenMac(decrypt, mac),
            };
            (packet_decrypt, mac_algo.tag_len)
        },
        CipherAlgoVariant::Aead(ref aead_algo) => {
            let decrypt = (aead_algo.make_decrypt)(&cipher_key, &cipher_iv);
            (PacketDecrypt::Aead(decrypt), aead_algo.tag_len)
        },
    };

    st.codec.recv_pipe.set_decrypt(packet_decrypt, cipher_algo.block_len, tag_len);

    log::debug!("received SSH_MSG_NEWKEYS and applied new keys");
    st.negotiate_st.new_keys_recvd = true;
    Ok(None)
}

fn send_new_keys(st: &mut ClientState) {
    let algos = st.negotiate_st.algos.as_ref().unwrap();

    let cipher_algo = algos.cipher_cts;
    let cipher_key = derive_key(st, b'C', cipher_algo.key_len);
    let cipher_iv = derive_key(st, b'A', cipher_algo.iv_len);

    let (packet_encrypt, tag_len) = match cipher_algo.variant {
        CipherAlgoVariant::Standard(ref standard_algo) => {
            let encrypt = (standard_algo.make_encrypt)(&cipher_key, &cipher_iv);

            let mac_algo = algos.mac_cts;
            let mac_key = derive_key(st, b'E', mac_algo.key_len);
            let mac = (mac_algo.make_mac)(&mac_key);

            let packet_encrypt = match mac_algo.variant {
                MacAlgoVariant::EncryptAndMac => PacketEncrypt::EncryptAndMac(encrypt, mac),
                MacAlgoVariant::EncryptThenMac => PacketEncrypt::EncryptThenMac(encrypt, mac),
            };
            (packet_encrypt, mac_algo.tag_len)
        },
        CipherAlgoVariant::Aead(ref aead_algo) => {
            let encrypt = (aead_algo.make_encrypt)(&cipher_key, &cipher_iv);
            (PacketEncrypt::Aead(encrypt), aead_algo.tag_len)
        },
    };

    let mut payload = PacketEncode::new();
    payload.put_u8(msg::NEWKEYS);
    st.codec.send_pipe.feed_packet(&payload.finish());

    st.codec.send_pipe.set_encrypt(packet_encrypt, cipher_algo.block_len, tag_len);
    log::debug!("sending SSH_MSG_NEWKEYS and applied new keys");
}

fn derive_key(st: &ClientState, key_type: u8, key_len: usize) -> Vec<u8> {
    // RFC 4253, section 7.2

    let kex = st.negotiate_st.kex.as_deref().unwrap();
    let kex_output = st.negotiate_st.kex_output.as_ref().unwrap();
    let session_id = st.session_id.as_ref().unwrap();

    let mut to_hash_prefix = PacketEncode::new();
    to_hash_prefix.put_biguint(&kex_output.shared_secret);
    to_hash_prefix.put_raw(&kex_output.exchange_hash);
    
    let mut key = {
        let mut to_hash = to_hash_prefix.clone();
        to_hash.put_u8(key_type);
        to_hash.put_raw(session_id);
        kex.compute_hash(&to_hash.finish())
    };

    while key.len() < key_len {
        let mut to_hash = to_hash_prefix.clone();
        to_hash.put_raw(&key);
        key.extend_from_slice(&kex.compute_hash(&to_hash.finish()));
    }

    key.truncate(key_len);
    key
}

fn maybe_send_ext_info(st: &mut ClientState) -> Result<()> {
    let ext_info_s = st.negotiate_st.their_kex_init.as_ref().unwrap().kex_algos.iter()
        .any(|name| name == "ext-info-s");
    if !st.last_kex.done && ext_info_s {
        ext::send_ext_info(st);
    }
    Ok(())
}


pub(super) fn is_ready(st: &ClientState) -> bool {
    matches!(st.negotiate_st.state, State::Idle)
}

pub(super) fn start_kex(st: &mut ClientState, done_tx: Option<oneshot::Sender<Result<()>>>) {
    if matches!(st.negotiate_st.state, State::Idle) {
        st.negotiate_st.state = State::KexInit;
        client_state::wakeup_client(st);
    }
    if let Some(done_tx) = done_tx {
        st.negotiate_st.done_txs.push(done_tx);
    }
}
