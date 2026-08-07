use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use whatsapp_rust::async_trait;
use whatsapp_rust::bytes::Bytes;
use whatsapp_rust::serde_json;

use whatsapp_rust::wacore::appstate::hash::HashState;
use whatsapp_rust::wacore::appstate::processor::AppStateMutationMAC;
use whatsapp_rust::wacore::store::Device;
use whatsapp_rust::wacore::store::error::Result;
use whatsapp_rust::wacore::store::traits::*;

/// In-memory storage backend for ESP32.
///
/// Stores everything in HashMaps protected by a std Mutex.
/// Sufficient for a single session; data is lost on reboot.
pub struct MemoryStore {
    inner: Mutex<StoreInner>,
}

/// `MsgSecretStore` key (chat, sender, msg_id) and value (secret, expires_at, message_ts).
///
/// The key parts are `Arc<str>` so a batch of history-sync entries — which share
/// one `chat`/`sender` allocation across every row — stores refcount clones
/// instead of re-allocating both JID strings per message.
type MsgSecretKey = (Arc<str>, Arc<str>, Arc<str>);
/// `secret` is the protocol-sized [`MessageSecret`] array (32 bytes) since 0.7.0,
/// so a row costs no separate heap allocation for the secret itself.
type MsgSecretValue = (MessageSecret, i64, i64);

#[derive(Default)]
struct StoreInner {
    device: Option<Device>,
    device_id_counter: i32,

    // SignalStore. sessions/prekeys are read per-device on every encrypt/decrypt and
    // the trait returns Bytes, so store them as Bytes: writers copy once, readers
    // hand back a refcount clone (no per-read heap copy).
    identities: HashMap<String, Vec<u8>>,
    sessions: HashMap<String, Bytes>,
    prekeys: HashMap<u32, Bytes>,
    signed_prekeys: HashMap<u32, Vec<u8>>,
    sender_keys: HashMap<String, Vec<u8>>,

    // AppSyncStore
    sync_keys: HashMap<Vec<u8>, AppStateSyncKey>,
    latest_sync_key_id: Option<Vec<u8>>,
    versions: HashMap<String, HashState>,
    mutation_macs: HashMap<String, HashMap<Vec<u8>, Vec<u8>>>,

    // ProtocolStore
    sender_key_devices: HashMap<String, Vec<(String, bool)>>,
    lid_mappings: HashMap<String, LidPnMappingEntry>,
    pn_mappings: HashMap<String, String>, // phone -> lid
    base_keys: HashMap<String, Vec<u8>>,
    device_lists: HashMap<String, DeviceListRecord>,
    tc_tokens: HashMap<String, TcTokenEntry>,
    sent_messages: HashMap<String, (Vec<u8>, i64)>, // key -> (payload, timestamp)

    // MsgSecretStore: keyed by (chat, sender, msg_id)
    msg_secrets: HashMap<MsgSecretKey, MsgSecretValue>,
}

/// Recover from mutex poisoning (a thread panicked while holding the lock).
/// Logs the event so panics don't go unnoticed.
fn recover_poisoned<T>(
    e: std::sync::PoisonError<std::sync::MutexGuard<'_, T>>,
) -> std::sync::MutexGuard<'_, T> {
    log::warn!("Mutex was poisoned (a thread panicked), recovering");
    e.into_inner()
}

/// Build the composite map key shared by the base-key and sent-message stores.
/// The writer and reader must format the key identically, so centralize it here.
/// A divergence would silently turn every lookup into a miss.
fn composite_key(a: &str, b: &str) -> String {
    format!("{a}:{b}")
}

/// Owned lookup key for `msg_secrets`. `std`'s `HashMap` cannot borrow a
/// `(&str, &str, &str)` as a `(Arc<str>, Arc<str>, Arc<str>)`, so a read builds
/// three short-lived `Arc<str>`s. Only add-on decryption (reactions, poll votes,
/// edits) reads this map, so the cost is off the hot receive path — the write
/// side, which does run per history-sync message, keeps the shared allocations.
fn msg_secret_key(chat: &str, sender: &str, msg_id: &str) -> MsgSecretKey {
    (Arc::from(chat), Arc::from(sender), Arc::from(msg_id))
}

impl MemoryStore {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(StoreInner::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, StoreInner> {
        self.inner.lock().unwrap_or_else(recover_poisoned)
    }

    /// Stats for the admin API.
    pub fn stats(&self) -> StoreStats {
        let s = self.lock();
        StoreStats {
            sessions: s.sessions.len(),
            identities: s.identities.len(),
            prekeys: s.prekeys.len(),
            sender_keys: s.sender_keys.len(),
            device_exists: s.device.is_some(),
        }
    }

    /// List all signal session addresses.
    pub fn list_sessions(&self) -> Vec<String> {
        self.lock().sessions.keys().cloned().collect()
    }

    /// Clear all signal sessions. Returns count deleted.
    pub fn clear_sessions(&self) -> usize {
        let mut s = self.lock();
        let count = s.sessions.len();
        s.sessions.clear();
        count
    }

    /// Full reset: clear everything by reassigning a fresh default. The field set
    /// lives in exactly one place now (`#[derive(Default)]` on `StoreInner`), so a
    /// newly added store field can never be silently left behind on a factory reset.
    pub fn reset(&self) {
        *self.lock() = StoreInner::default();
    }
}

pub struct StoreStats {
    pub sessions: usize,
    pub identities: usize,
    pub prekeys: usize,
    pub sender_keys: usize,
    pub device_exists: bool,
}

/// Shared state between event handler and admin server.
pub struct DeviceStatus {
    inner: Mutex<DeviceStatusInner>,
}

struct DeviceStatusInner {
    qr_code: Option<String>,
    connected: bool,
    pn: Option<String>,
    lid: Option<String>,
}

impl DeviceStatus {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(DeviceStatusInner {
                qr_code: None,
                connected: false,
                pn: None,
                lid: None,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, DeviceStatusInner> {
        self.inner.lock().unwrap_or_else(recover_poisoned)
    }

    pub fn set_qr_code(&self, code: String) {
        let mut s = self.lock();
        s.qr_code = Some(code);
        s.connected = false;
    }

    pub fn set_connected(&self, pn: Option<String>, lid: Option<String>) {
        let mut s = self.lock();
        s.qr_code = None;
        s.connected = true;
        s.pn = pn;
        s.lid = lid;
    }

    /// Socket dropped. Keeps pn/lid: the device is still paired, just offline,
    /// and the dashboard should keep showing which account it is.
    pub fn set_disconnected(&self) {
        self.lock().connected = false;
    }

    /// Unpaired by the server (or locally). The identity is gone, so clear it —
    /// otherwise the dashboard would show a stale account next to the new QR.
    pub fn set_logged_out(&self) {
        let mut s = self.lock();
        s.connected = false;
        s.pn = None;
        s.lid = None;
    }

    pub fn to_json(&self) -> String {
        let s = self.lock();
        serde_json::json!({
            "qr_code": s.qr_code,
            "connected": s.connected,
            "pn": s.pn,
            "lid": s.lid,
        })
        .to_string()
    }
}

#[async_trait]
impl SignalStore for MemoryStore {
    async fn put_identity(&self, address: &str, key: [u8; 32]) -> Result<()> {
        self.lock()
            .identities
            .insert(address.to_string(), key.to_vec());
        Ok(())
    }

    async fn load_identity(&self, address: &str) -> Result<Option<[u8; 32]>> {
        Ok(self
            .lock()
            .identities
            .get(address)
            .and_then(|v| <[u8; 32]>::try_from(v.as_slice()).ok()))
    }

    async fn delete_identity(&self, address: &str) -> Result<()> {
        self.lock().identities.remove(address);
        Ok(())
    }

    async fn get_session(&self, address: &str) -> Result<Option<Bytes>> {
        Ok(self.lock().sessions.get(address).cloned())
    }

    async fn put_session(&self, address: &str, session: &[u8]) -> Result<()> {
        self.lock()
            .sessions
            .insert(address.to_string(), Bytes::copy_from_slice(session));
        Ok(())
    }

    async fn delete_session(&self, address: &str) -> Result<()> {
        self.lock().sessions.remove(address);
        Ok(())
    }

    /// The trait default answers a conservative `true`, which makes the client
    /// run a full per-device PN->LID migration scan for every user it has never
    /// exchanged Signal state with. Both maps are small here, so answering for
    /// real is cheap and skips that scan outright.
    async fn has_signal_state_for_user(&self, user: &str) -> Result<bool> {
        // Addresses are `user@server` (device 0) or `user:dev@server`, so a bare
        // `starts_with` would also match a longer user id with the same prefix.
        fn matches(address: &str, user: &str) -> bool {
            address
                .strip_prefix(user)
                .is_some_and(|rest| rest.starts_with('@') || rest.starts_with(':'))
        }
        let s = self.lock();
        Ok(s.sessions.keys().any(|k| matches(k, user))
            || s.identities.keys().any(|k| matches(k, user)))
    }

    async fn store_prekey(&self, id: u32, record: &[u8], _uploaded: bool) -> Result<()> {
        self.lock()
            .prekeys
            .insert(id, Bytes::copy_from_slice(record));
        Ok(())
    }

    async fn load_prekey(&self, id: u32) -> Result<Option<Bytes>> {
        Ok(self.lock().prekeys.get(&id).cloned())
    }

    async fn remove_prekey(&self, id: u32) -> Result<()> {
        self.lock().prekeys.remove(&id);
        Ok(())
    }

    async fn get_max_prekey_id(&self) -> Result<u32> {
        Ok(self.lock().prekeys.keys().copied().max().unwrap_or(0))
    }

    async fn mark_prekeys_uploaded(&self, _ids: &[u32]) -> Result<()> {
        Ok(()) // in-memory store does not track prekey upload status
    }

    async fn store_signed_prekey(&self, id: u32, record: &[u8]) -> Result<()> {
        self.lock().signed_prekeys.insert(id, record.to_vec());
        Ok(())
    }

    async fn load_signed_prekey(&self, id: u32) -> Result<Option<Vec<u8>>> {
        Ok(self.lock().signed_prekeys.get(&id).cloned())
    }

    async fn load_all_signed_prekeys(&self) -> Result<Vec<(u32, Vec<u8>)>> {
        Ok(self
            .lock()
            .signed_prekeys
            .iter()
            .map(|(&k, v)| (k, v.clone()))
            .collect())
    }

    async fn remove_signed_prekey(&self, id: u32) -> Result<()> {
        self.lock().signed_prekeys.remove(&id);
        Ok(())
    }

    async fn put_sender_key(&self, address: &str, record: &[u8]) -> Result<()> {
        self.lock()
            .sender_keys
            .insert(address.to_string(), record.to_vec());
        Ok(())
    }

    async fn get_sender_key(&self, address: &str) -> Result<Option<Vec<u8>>> {
        Ok(self.lock().sender_keys.get(address).cloned())
    }

    async fn delete_sender_key(&self, address: &str) -> Result<()> {
        self.lock().sender_keys.remove(address);
        Ok(())
    }
}

#[async_trait]
impl AppSyncStore for MemoryStore {
    async fn get_sync_key(&self, key_id: &[u8]) -> Result<Option<AppStateSyncKey>> {
        Ok(self.lock().sync_keys.get(key_id).cloned())
    }

    async fn set_sync_key(&self, key_id: &[u8], key: AppStateSyncKey) -> Result<()> {
        let mut s = self.lock();
        s.latest_sync_key_id = Some(key_id.to_vec());
        s.sync_keys.insert(key_id.to_vec(), key);
        Ok(())
    }

    async fn get_version(&self, name: &str) -> Result<HashState> {
        Ok(self.lock().versions.get(name).cloned().unwrap_or_default())
    }

    async fn set_version(&self, name: &str, state: HashState) -> Result<()> {
        self.lock().versions.insert(name.to_string(), state);
        Ok(())
    }

    async fn put_mutation_macs(
        &self,
        name: &str,
        _version: u64,
        mutations: &[AppStateMutationMAC],
    ) -> Result<()> {
        let mut s = self.lock();
        let map = s.mutation_macs.entry(name.to_string()).or_default();
        for m in mutations {
            map.insert(m.index_mac.clone(), m.value_mac.clone());
        }
        Ok(())
    }

    async fn get_mutation_mac(&self, name: &str, index_mac: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self
            .lock()
            .mutation_macs
            .get(name)
            .and_then(|m| m.get(index_mac).cloned()))
    }

    async fn delete_mutation_macs(&self, name: &str, index_macs: &[Vec<u8>]) -> Result<()> {
        let mut s = self.lock();
        if let Some(map) = s.mutation_macs.get_mut(name) {
            for mac in index_macs {
                map.remove(mac);
            }
        }
        Ok(())
    }

    async fn clear_mutation_macs(&self, name: &str) -> Result<()> {
        self.lock().mutation_macs.remove(name);
        Ok(())
    }

    async fn get_latest_sync_key_id(&self) -> Result<Option<Vec<u8>>> {
        Ok(self.lock().latest_sync_key_id.clone())
    }
}

#[async_trait]
impl ProtocolStore for MemoryStore {
    async fn get_sender_key_devices(&self, group_jid: &str) -> Result<Vec<(String, bool)>> {
        Ok(self
            .lock()
            .sender_key_devices
            .get(group_jid)
            .cloned()
            .unwrap_or_default())
    }

    async fn set_sender_key_status(&self, group_jid: &str, entries: &[(&str, bool)]) -> Result<()> {
        let mut s = self.lock();
        let devices = s
            .sender_key_devices
            .entry(group_jid.to_string())
            .or_default();
        for &(jid, status) in entries {
            if let Some(entry) = devices.iter_mut().find(|(j, _)| j == jid) {
                entry.1 = status;
            } else {
                devices.push((jid.to_string(), status));
            }
        }
        Ok(())
    }

    async fn clear_sender_key_devices(&self, group_jid: &str) -> Result<()> {
        self.lock().sender_key_devices.remove(group_jid);
        Ok(())
    }

    async fn clear_all_sender_key_devices(&self) -> Result<()> {
        self.lock().sender_key_devices.clear();
        Ok(())
    }

    async fn delete_sender_key_device_rows(&self, device_jids: &[&str]) -> Result<()> {
        let targets: std::collections::HashSet<&str> = device_jids.iter().copied().collect();
        let mut s = self.lock();
        for devices in s.sender_key_devices.values_mut() {
            devices.retain(|(jid, _)| !targets.contains(jid.as_str()));
        }
        Ok(())
    }

    async fn get_lid_mapping(&self, lid: &str) -> Result<Option<LidPnMappingEntry>> {
        Ok(self.lock().lid_mappings.get(lid).cloned())
    }

    async fn get_pn_mapping(&self, phone: &str) -> Result<Option<LidPnMappingEntry>> {
        let s = self.lock();
        let lid = s.pn_mappings.get(phone).cloned();
        Ok(lid.and_then(|l| s.lid_mappings.get(&l).cloned()))
    }

    async fn put_lid_mapping(&self, entry: &LidPnMappingEntry) -> Result<()> {
        let mut s = self.lock();
        s.pn_mappings
            .insert(entry.phone_number.clone(), entry.lid.clone());
        s.lid_mappings.insert(entry.lid.clone(), entry.clone());
        Ok(())
    }

    async fn get_all_lid_mappings(&self) -> Result<Vec<LidPnMappingEntry>> {
        Ok(self.lock().lid_mappings.values().cloned().collect())
    }

    async fn save_base_key(&self, address: &str, message_id: &str, base_key: &[u8]) -> Result<()> {
        let key = composite_key(address, message_id);
        self.lock().base_keys.insert(key, base_key.to_vec());
        Ok(())
    }

    async fn has_same_base_key(
        &self,
        address: &str,
        message_id: &str,
        current_base_key: &[u8],
    ) -> Result<bool> {
        let key = composite_key(address, message_id);
        Ok(self
            .lock()
            .base_keys
            .get(&key)
            .is_some_and(|k| k == current_base_key))
    }

    async fn delete_base_key(&self, address: &str, message_id: &str) -> Result<()> {
        let key = composite_key(address, message_id);
        self.lock().base_keys.remove(&key);
        Ok(())
    }

    async fn update_device_list(&self, record: DeviceListRecord) -> Result<()> {
        self.lock().device_lists.insert(record.user.clone(), record);
        Ok(())
    }

    async fn get_devices(&self, user: &str) -> Result<Option<DeviceListRecord>> {
        Ok(self.lock().device_lists.get(user).cloned())
    }

    async fn delete_devices(&self, user: &str) -> Result<()> {
        self.lock().device_lists.remove(user);
        Ok(())
    }

    async fn get_tc_token(&self, jid: &str) -> Result<Option<TcTokenEntry>> {
        Ok(self.lock().tc_tokens.get(jid).cloned())
    }

    async fn put_tc_token(&self, jid: &str, entry: &TcTokenEntry) -> Result<()> {
        self.lock().tc_tokens.insert(jid.to_string(), entry.clone());
        Ok(())
    }

    async fn delete_tc_token(&self, jid: &str) -> Result<()> {
        self.lock().tc_tokens.remove(jid);
        Ok(())
    }

    async fn get_all_tc_token_jids(&self) -> Result<Vec<String>> {
        Ok(self.lock().tc_tokens.keys().cloned().collect())
    }

    /// 0.7.0 split the single cutoff in two: a row now also carries a
    /// `sender_timestamp` bucket (the last token WE issued to that contact),
    /// which lives on its own retention clock. Drop a row only when BOTH sides
    /// are stale — pruning on the received token alone would throw away recent
    /// sender state and make us re-issue tokens we already sent.
    async fn delete_expired_tc_tokens(&self, token_cutoff: i64, sender_cutoff: i64) -> Result<u32> {
        let mut s = self.lock();
        let before = s.tc_tokens.len();
        s.tc_tokens.retain(|_, v| {
            // An empty `token` is a placeholder row written by the sender path;
            // it never counts as live received state whatever its timestamp.
            let token_live = !v.token.is_empty() && v.token_timestamp >= token_cutoff;
            let sender_live = v.sender_timestamp.is_some_and(|ts| ts >= sender_cutoff);
            token_live || sender_live
        });
        Ok((before - s.tc_tokens.len()) as u32)
    }

    /// Overridden for atomicity. The trait default is a read-modify-write across
    /// two awaits, so a concurrent `put_tc_token` from the notification path can
    /// land in between and be clobbered by the placeholder this writes. Holding
    /// the store mutex for the whole update makes the upsert indivisible, which
    /// is what the trait asks backends to provide.
    async fn touch_tc_token_sender_timestamp(
        &self,
        jid: &str,
        sender_timestamp: i64,
    ) -> Result<()> {
        let mut s = self.lock();
        match s.tc_tokens.get_mut(jid) {
            // Monotonic: the bucket only ever moves forward, so post-send
            // issuance and history sync converge whatever order they arrive in.
            Some(entry) => {
                entry.sender_timestamp = Some(
                    entry
                        .sender_timestamp
                        .map_or(sender_timestamp, |e| e.max(sender_timestamp)),
                );
            }
            None => {
                s.tc_tokens.insert(
                    jid.to_string(),
                    TcTokenEntry {
                        token: Vec::new(),
                        token_timestamp: sender_timestamp,
                        sender_timestamp: Some(sender_timestamp),
                    },
                );
            }
        }
        Ok(())
    }

    /// Symmetric counterpart of [`touch_tc_token_sender_timestamp`]: each writer
    /// owns one field, so the notification path never drops a sender bucket the
    /// issuance path wrote. Newer-wins — a stale token must not clobber a fresher
    /// one, but a byte-less placeholder never blocks the first real token.
    /// Overridden for the same atomicity reason as above.
    async fn store_received_tc_token(
        &self,
        jid: &str,
        token: &[u8],
        token_timestamp: i64,
    ) -> Result<()> {
        let mut s = self.lock();
        match s.tc_tokens.get_mut(jid) {
            Some(entry) => {
                if !entry.token.is_empty() && token_timestamp < entry.token_timestamp {
                    return Ok(());
                }
                entry.token = token.to_vec();
                entry.token_timestamp = token_timestamp;
            }
            None => {
                s.tc_tokens.insert(
                    jid.to_string(),
                    TcTokenEntry {
                        token: token.to_vec(),
                        token_timestamp,
                        sender_timestamp: None,
                    },
                );
            }
        }
        Ok(())
    }

    async fn store_sent_message(
        &self,
        chat_jid: &str,
        message_id: &str,
        payload: &[u8],
    ) -> Result<()> {
        let key = composite_key(chat_jid, message_id);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        self.lock()
            .sent_messages
            .insert(key, (payload.to_vec(), now));
        Ok(())
    }

    async fn take_sent_message(&self, chat_jid: &str, message_id: &str) -> Result<Option<Vec<u8>>> {
        let key = composite_key(chat_jid, message_id);
        Ok(self.lock().sent_messages.remove(&key).map(|(p, _)| p))
    }

    async fn delete_expired_sent_messages(&self, cutoff_timestamp: i64) -> Result<u32> {
        let mut s = self.lock();
        let before = s.sent_messages.len();
        s.sent_messages.retain(|_, (_, ts)| *ts >= cutoff_timestamp);
        Ok((before - s.sent_messages.len()) as u32)
    }
}

#[async_trait]
impl DeviceStore for MemoryStore {
    async fn save(&self, device: &Device) -> Result<()> {
        self.lock().device = Some(device.clone());
        Ok(())
    }

    async fn load(&self) -> Result<Option<Device>> {
        Ok(self.lock().device.clone())
    }

    async fn exists(&self) -> Result<bool> {
        Ok(self.lock().device.is_some())
    }

    async fn create(&self) -> Result<i32> {
        let mut s = self.lock();
        s.device_id_counter += 1;
        Ok(s.device_id_counter)
    }
}

#[async_trait]
impl MsgSecretStore for MemoryStore {
    async fn put_msg_secrets(&self, entries: Vec<MsgSecretEntry>) -> Result<usize> {
        let mut s = self.lock();
        let count = entries.len();
        for e in entries {
            // Moves the entry's Arc<str> parts into the key: no re-allocation,
            // and a history batch keeps sharing one chat/sender string.
            let key: MsgSecretKey = (e.chat, e.sender, e.msg_id);
            match s.msg_secrets.get_mut(&key) {
                // On conflict, never shrink the retention window nor clobber a known parent ts.
                Some(existing) => {
                    existing.0 = e.secret;
                    existing.1 = merge_msg_secret_expiry(existing.1, e.expires_at);
                    existing.2 = merge_msg_secret_message_ts(existing.2, e.message_ts);
                }
                None => {
                    s.msg_secrets
                        .insert(key, (e.secret, e.expires_at, e.message_ts));
                }
            }
        }
        Ok(count)
    }

    async fn get_msg_secret(
        &self,
        chat: &str,
        sender: &str,
        msg_id: &str,
    ) -> Result<Option<Vec<u8>>> {
        let key = msg_secret_key(chat, sender, msg_id);
        Ok(self
            .lock()
            .msg_secrets
            .get(&key)
            .map(|(secret, _, _)| secret.to_vec()))
    }

    async fn get_msg_secret_with_ts(
        &self,
        chat: &str,
        sender: &str,
        msg_id: &str,
    ) -> Result<Option<(Vec<u8>, i64)>> {
        let key = msg_secret_key(chat, sender, msg_id);
        Ok(self
            .lock()
            .msg_secrets
            .get(&key)
            .map(|(secret, _, ts)| (secret.to_vec(), *ts)))
    }

    async fn delete_expired_msg_secrets(&self, cutoff_timestamp: i64) -> Result<u32> {
        let mut s = self.lock();
        let before = s.msg_secrets.len();
        // expires_at == 0 means "never expire".
        s.msg_secrets
            .retain(|_, (_, expires_at, _)| *expires_at == 0 || *expires_at > cutoff_timestamp);
        Ok((before - s.msg_secrets.len()) as u32)
    }
}
