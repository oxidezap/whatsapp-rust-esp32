//! Storage backend: the linked device, its Signal state and the app-state sync
//! keys live in the `wa_store` NVS partition and survive reboots and power
//! loss, so the chip comes back as the same linked device without a new QR
//! scan. Everything else the `Backend` traits cover (app-state hash versions,
//! LID mappings, device lists, tokens, message secrets) is a cache the
//! protocol can rebuild, and stays in RAM.
//!
//! Layout on flash: one NVS namespace per record kind, one blob per record.
//! NVS keys are 15 characters at most, so a record is named by a truncated
//! SHA-256 of its logical key (a Signal address, a prekey id) and carries that
//! logical key in a small header, which is how a name collision is detected
//! instead of silently overwriting a neighbour. Every write goes through one
//! `wa-nvs` thread with an internal-RAM stack, because the flash cache is off
//! while flash is being written and a PSRAM stack would fault.
//!
//! Reads are served from the in-RAM mirror that is filled once at boot; a
//! write updates flash first and the mirror only once flash has committed, so
//! RAM never claims more than flash holds.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use esp_idf_svc::handle::RawHandle;
use esp_idf_svc::nvs::{EspCustomNvs, EspCustomNvsPartition, EspNvs, NvsDataType};
use sha2::{Digest, Sha256};
use whatsapp_rust::async_trait;
use whatsapp_rust::bytes::Bytes;
use whatsapp_rust::serde_json;

use whatsapp_rust::wacore::appstate::hash::HashState;
use whatsapp_rust::wacore::appstate::processor::AppStateMutationMAC;
use whatsapp_rust::wacore::store::error::{Result, StoreError};
use whatsapp_rust::wacore::store::traits::*;
use whatsapp_rust::wacore::store::Device;

const RECORD_MAGIC: &[u8; 3] = b"WA1";
const RECORD_HEADER_LEN: usize = 5;
const MAX_LOGICAL_KEY_LEN: usize = 512;
const MAX_DEVICE_LEN: usize = 64 * 1024;
const MAX_SIGNAL_RECORD_LEN: usize = 64 * 1024;
const MAX_IDENTITIES: usize = 2048;
const MAX_SESSIONS: usize = 2048;
const MAX_PREKEYS: usize = 256;
const MAX_SIGNED_PREKEYS: usize = 16;
const MAX_SENDER_KEYS: usize = 2048;
// The Signal caps are a sanity bound on what a namespace may hold; the 1 MB
// partition fills long before any of them is reached in practice. Sync keys are
// the exception worth sizing deliberately: the store trait has `remove_prekey`
// and `remove_signed_prekey` but no delete for a sync key, so they only ever
// accumulate, and the write that hits this cap fails app-state sync for good.
// They are ~200 bytes each, so a generous bound costs a rounding error of flash.
const MAX_SYNC_KEYS: usize = 256;

/// Flash-backed storage for the linked device and its cryptographic state.
pub struct NvsStore {
    inner: Mutex<StoreInner>,
    flash: FlashWorker,
    /// Serializes every flash-touching operation, and is what `seal_writes`
    /// takes so that no write can slip in between the seal and the erase.
    operation: Mutex<()>,
    accepting_writes: AtomicBool,
}

struct FlashNamespaces {
    control: EspCustomNvs,
    device: EspCustomNvs,
    identities: EspCustomNvs,
    sessions: EspCustomNvs,
    prekeys: EspCustomNvs,
    signed_prekeys: EspCustomNvs,
    sender_keys: EspCustomNvs,
    sync_keys: EspCustomNvs,
}

type FlashJob = Box<dyn FnOnce(&FlashNamespaces) + Send>;

struct FlashWorker {
    jobs: mpsc::SyncSender<FlashJob>,
}

impl FlashWorker {
    fn start(flash: FlashNamespaces) -> anyhow::Result<Self> {
        let (jobs, receiver) = mpsc::sync_channel::<FlashJob>(1);
        // Writing flash switches the cache off, so every byte this thread touches
        // has to be in internal RAM; a stack in PSRAM would fault the moment the
        // write begins. Not configurable for that reason, and the one stack that
        // costs internal DRAM on every board.
        //
        // Which is why it is also the one worth measuring. The jobs that run here
        // are shallow and bounded -- put/delete one record, or erase a namespace
        // -- and none of them puts a record on the stack: `read_blob` and
        // `encode_record` both build `Vec`s. The boot replay, the only thing that
        // walks the whole store, runs on the ESP-IDF main task before this worker
        // exists (hence CONFIG_ESP_MAIN_TASK_STACK_SIZE), not here. Measured peak
        // on the emulated ESP32-C3, across pairing writes and a factory reset, is
        // 2,596 bytes (see docs/esp32c3.md). A 6 KB stack keeps well over twice
        // that, and a board with PSRAM keeps the 32 KB it was tested on since it
        // has DRAM to spare.
        let thread = esp_idf_svc::hal::task::thread::ThreadSpawnConfiguration {
            name: Some(c"wa-nvs"),
            stack_size: crate::runtime::by_ram(32 * 1024, 6 * 1024),
            priority: 5,
            inherit: false,
            pin_to_core: None,
            stack_alloc_caps: enumset::enum_set!(
                esp_idf_svc::hal::task::thread::MallocCap::Internal
                    | esp_idf_svc::hal::task::thread::MallocCap::Cap8bit
            ),
        };
        crate::runtime::spawn_thread(&thread, move || {
            while let Ok(job) = receiver.recv() {
                job(&flash);
            }
        })
        .map_err(|error| anyhow::anyhow!("could not start the NVS worker: {error}"))?;
        Ok(Self { jobs })
    }

    fn run<T, F>(&self, operation: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&FlashNamespaces) -> Result<T> + Send + 'static,
    {
        let (result_tx, result_rx) = mpsc::sync_channel(0);
        self.jobs
            .send(Box::new(move |flash| {
                let _ = result_tx.send(operation(flash));
            }))
            .map_err(|_| StoreError::Io(std::io::Error::other("WhatsApp NVS worker stopped")))?;
        result_rx.recv().map_err(|_| {
            StoreError::Io(std::io::Error::other(
                "WhatsApp NVS worker dropped a result",
            ))
        })?
    }
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

    // SignalStore (persisted). sessions/prekeys are read per-device on every
    // encrypt/decrypt and the trait returns Bytes, so store them as Bytes:
    // writers copy once, readers hand back a refcount clone.
    identities: HashMap<String, [u8; 32]>,
    sessions: HashMap<String, Bytes>,
    prekeys: HashMap<u32, Bytes>,
    signed_prekeys: HashMap<u32, Vec<u8>>,
    sender_keys: HashMap<String, Vec<u8>>,

    // AppSyncStore. The keys are persisted (without them a rebooted device has
    // to ask the phone for every key again, and the phone must be online to
    // answer); the hash versions and MACs are a rebuildable cache.
    sync_keys: HashMap<Vec<u8>, AppStateSyncKey>,
    latest_sync_key_id: Option<Vec<u8>>,
    versions: HashMap<String, HashState>,
    mutation_macs: HashMap<String, HashMap<Vec<u8>, Vec<u8>>>,

    // ProtocolStore (RAM only)
    sender_key_devices: HashMap<String, Vec<(String, bool)>>,
    lid_mappings: HashMap<String, LidPnMappingEntry>,
    pn_mappings: HashMap<String, String>, // phone -> lid
    base_keys: HashMap<String, Vec<u8>>,
    // Keyed by the record's own `Arc<str>`: the trait hands the user out as
    // `Arc<str>` now, and a `&str` lookup borrows through it, so no `String`
    // copy of the user is made on write or read.
    device_lists: HashMap<Arc<str>, DeviceListRecord>,
    tc_tokens: HashMap<String, TcTokenEntry>,
    sent_messages: HashMap<String, (Vec<u8>, i64)>, // key -> (payload, timestamp)

    // MsgSecretStore: keyed by (chat, sender, msg_id)
    msg_secrets: HashMap<MsgSecretKey, MsgSecretValue>,
}

impl FlashNamespaces {
    fn open(partition: EspCustomNvsPartition) -> anyhow::Result<Self> {
        Ok(Self {
            control: EspNvs::new(partition.clone(), "control", true)?,
            device: EspNvs::new(partition.clone(), "device", true)?,
            identities: EspNvs::new(partition.clone(), "identity", true)?,
            sessions: EspNvs::new(partition.clone(), "session", true)?,
            prekeys: EspNvs::new(partition.clone(), "prekey", true)?,
            signed_prekeys: EspNvs::new(partition.clone(), "signedpre", true)?,
            sender_keys: EspNvs::new(partition.clone(), "senderkey", true)?,
            sync_keys: EspNvs::new(partition, "synckey", true)?,
        })
    }

    fn load_device(&self) -> Result<Option<Device>> {
        let Some(record) = read_blob(
            &self.device,
            "current",
            RECORD_HEADER_LEN + b"device".len() + MAX_DEVICE_LEN,
        )?
        else {
            return Ok(None);
        };
        let (logical_key, payload) = decode_record(&record)?;
        if logical_key != b"device" {
            return Err(StoreError::Validation(
                "device record contains the wrong logical key".to_string(),
            ));
        }
        serde_json::from_slice(payload)
            .map(Some)
            .map_err(|error| StoreError::Serialization(Box::new(error)))
    }

    fn save_device(&self, device: &Device) -> Result<()> {
        let payload = serde_json::to_vec(device)
            .map_err(|error| StoreError::Serialization(Box::new(error)))?;
        if payload.len() > MAX_DEVICE_LEN {
            return Err(StoreError::Validation(format!(
                "device record is too large: {} bytes",
                payload.len()
            )));
        }
        let record = encode_record(b"device", &payload)?;
        // The device record is the largest single thing this store writes, and on
        // a board without PSRAM it is written while the WebSocket thread may be
        // reserving for an inbound frame -- the ESP32-C3 aborts on a 32,300-byte
        // reserve that had 51,200 contiguous a moment earlier, with this save the
        // only other thing in flight. Log the size so the two can be compared
        // instead of assumed.
        log::debug!(
            "device record: {} bytes ({} payload){}",
            record.len(),
            payload.len(),
            crate::metrics::heap_note()
        );
        set_blob(&self.device, "current", &record)
    }

    fn load_records(
        &self,
        namespace: &EspCustomNvs,
        label: &str,
        max_entries: usize,
        max_value_len: usize,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let names = record_names(namespace)?;
        if names.len() > max_entries {
            return Err(StoreError::Validation(format!(
                "{label} namespace has {} records; maximum is {max_entries}",
                names.len()
            )));
        }

        let mut records = Vec::with_capacity(names.len());
        for name in names {
            let record = read_blob(
                namespace,
                &name,
                RECORD_HEADER_LEN + MAX_LOGICAL_KEY_LEN + max_value_len,
            )?
            .ok_or_else(|| {
                StoreError::Validation(format!(
                    "{label} record '{name}' disappeared during startup"
                ))
            })?;
            let (logical_key, value) = decode_record(&record)?;
            if record_name(logical_key) != name {
                return Err(StoreError::Validation(format!(
                    "{label} record '{name}' has a mismatched logical key"
                )));
            }
            if value.len() > max_value_len {
                return Err(StoreError::Validation(format!(
                    "{label} record '{name}' is too large: {} bytes",
                    value.len()
                )));
            }
            records.push((logical_key.to_vec(), value.to_vec()));
        }
        Ok(records)
    }

    fn put_record(
        &self,
        namespace: &EspCustomNvs,
        logical_key: &[u8],
        value: &[u8],
        label: &str,
    ) -> Result<()> {
        if value.len() > MAX_SIGNAL_RECORD_LEN {
            return Err(StoreError::Validation(format!(
                "{label} record is too large: {} bytes",
                value.len()
            )));
        }
        let name = record_name(logical_key);
        if let Some(existing) = read_blob(
            namespace,
            &name,
            RECORD_HEADER_LEN + MAX_LOGICAL_KEY_LEN + MAX_SIGNAL_RECORD_LEN,
        )? {
            let (stored_key, _) = decode_record(&existing)?;
            if stored_key != logical_key {
                return Err(StoreError::Validation(format!(
                    "{label} NVS key collision at '{name}'"
                )));
            }
        }
        let record = encode_record(logical_key, value)?;
        set_blob(namespace, &name, &record)
    }

    fn delete_record(
        &self,
        namespace: &EspCustomNvs,
        logical_key: &[u8],
        label: &str,
    ) -> Result<()> {
        let name = record_name(logical_key);
        if let Some(existing) = read_blob(
            namespace,
            &name,
            RECORD_HEADER_LEN + MAX_LOGICAL_KEY_LEN + MAX_SIGNAL_RECORD_LEN,
        )? {
            let (stored_key, _) = decode_record(&existing)?;
            if stored_key != logical_key {
                return Err(StoreError::Validation(format!(
                    "{label} NVS key collision at '{name}'"
                )));
            }
            namespace.remove(&name).map_err(nvs_error)?;
            commit(namespace)?;
        }
        Ok(())
    }

    fn has_signal_records(&self) -> Result<bool> {
        for namespace in [
            &self.identities,
            &self.sessions,
            &self.prekeys,
            &self.signed_prekeys,
            &self.sender_keys,
            &self.sync_keys,
        ] {
            if !record_names(namespace)?.is_empty() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn erase_signal(&self) -> Result<()> {
        // Belongs with the keys it names, so it goes when they do.
        remove_blob(&self.control, "latestsync")?;
        erase_namespace(&self.identities, "identity")?;
        erase_namespace(&self.sessions, "session")?;
        erase_namespace(&self.prekeys, "prekey")?;
        erase_namespace(&self.signed_prekeys, "signed prekey")?;
        erase_namespace(&self.sender_keys, "sender key")?;
        erase_namespace(&self.sync_keys, "sync key")?;
        Ok(())
    }

    fn erase_all(&self) -> Result<()> {
        // Absence of the device is the durable reset tombstone. If power fails
        // after this commit, startup discards any remaining orphaned Signal rows.
        erase_namespace(&self.device, "device")?;
        self.erase_signal()
    }

    fn load_latest_sync_key_id(&self) -> Result<Option<Vec<u8>>> {
        let Some(record) = read_blob(
            &self.control,
            "latestsync",
            RECORD_HEADER_LEN + b"latestsync".len() + MAX_LOGICAL_KEY_LEN,
        )?
        else {
            return Ok(None);
        };
        let (logical_key, payload) = decode_record(&record)?;
        if logical_key != b"latestsync" {
            return Err(StoreError::Validation(
                "latest sync key record contains the wrong logical key".to_string(),
            ));
        }
        Ok(Some(payload.to_vec()))
    }

    fn save_latest_sync_key_id(&self, key_id: &[u8]) -> Result<()> {
        if key_id.len() > MAX_LOGICAL_KEY_LEN {
            return Err(StoreError::Validation(format!(
                "sync key id is too long: {} bytes",
                key_id.len()
            )));
        }
        let record = encode_record(b"latestsync", key_id)?;
        set_blob(&self.control, "latestsync", &record)
    }

    fn reset_pending(&self) -> Result<bool> {
        Ok(read_blob(&self.control, "reset", 1)?.is_some())
    }

    fn reset_all(&self) -> Result<()> {
        // This marker is outside every erased namespace. If power fails before
        // cleanup completes, startup resumes the reset instead of loading the
        // credential that the server already invalidated.
        set_blob(&self.control, "reset", b"1")?;
        self.complete_reset()
    }

    fn complete_reset(&self) -> Result<()> {
        self.erase_all()?;
        remove_blob(&self.control, "reset")
    }
}

fn nvs_error(error: esp_idf_svc::sys::EspError) -> StoreError {
    StoreError::Io(std::io::Error::other(format!("ESP NVS error: {error}")))
}

fn commit(namespace: &EspCustomNvs) -> Result<()> {
    let commit_result = unsafe { esp_idf_svc::sys::nvs_commit(namespace.handle()) };
    if let Some(error) = esp_idf_svc::sys::EspError::from(commit_result) {
        return Err(nvs_error(error));
    }
    Ok(())
}

fn set_blob(namespace: &EspCustomNvs, name: &str, value: &[u8]) -> Result<()> {
    let name = std::ffi::CString::new(name)
        .map_err(|error| StoreError::Validation(format!("invalid NVS record name: {error}")))?;
    let set_result = unsafe {
        esp_idf_svc::sys::nvs_set_blob(
            namespace.handle(),
            name.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
        )
    };
    if let Some(error) = esp_idf_svc::sys::EspError::from(set_result) {
        return Err(nvs_error(error));
    }
    commit(namespace)
}

fn remove_blob(namespace: &EspCustomNvs, name: &str) -> Result<()> {
    if namespace.blob_len(name).map_err(nvs_error)?.is_none() {
        return Ok(());
    }
    namespace.remove(name).map_err(nvs_error)?;
    commit(namespace)
}

fn erase_namespace(namespace: &EspCustomNvs, label: &str) -> Result<()> {
    let mut last_error = None;
    for attempt in 1..=3 {
        match namespace.erase_all().map_err(nvs_error) {
            Ok(()) => match record_names(namespace) {
                Ok(names) if names.is_empty() => return Ok(()),
                Ok(names) => {
                    last_error = Some(format!(
                        "verification found {} remaining records",
                        names.len()
                    ));
                }
                Err(error) => last_error = Some(error.to_string()),
            },
            Err(error) => last_error = Some(error.to_string()),
        }
        log::warn!("NVS {label} erase attempt {attempt} failed verification");
    }
    Err(StoreError::Io(std::io::Error::other(format!(
        "failed to erase NVS {label} namespace after 3 attempts: {}",
        last_error.unwrap_or_else(|| "unknown error".to_string())
    ))))
}

fn record_names(namespace: &EspCustomNvs) -> Result<Vec<String>> {
    let mut iterator = namespace.keys(Some(NvsDataType::Blob)).map_err(nvs_error)?;
    let mut names = Vec::new();
    while let Some((name, _)) = iterator.next_key() {
        names.push(name.to_string());
    }
    Ok(names)
}

fn read_blob(namespace: &EspCustomNvs, name: &str, max_len: usize) -> Result<Option<Vec<u8>>> {
    let Some(len) = namespace.blob_len(name).map_err(nvs_error)? else {
        return Ok(None);
    };
    if len > max_len {
        return Err(StoreError::Validation(format!(
            "NVS record '{name}' is too large: {len} bytes"
        )));
    }
    let mut data = vec![0; len];
    let actual_len = namespace
        .get_blob(name, &mut data)
        .map_err(nvs_error)?
        .ok_or_else(|| StoreError::Validation(format!("NVS record '{name}' disappeared")))?
        .len();
    data.truncate(actual_len);
    Ok(Some(data))
}

fn encode_record(logical_key: &[u8], value: &[u8]) -> Result<Vec<u8>> {
    if logical_key.len() > MAX_LOGICAL_KEY_LEN {
        return Err(StoreError::Validation(format!(
            "logical storage key is too long: {} bytes",
            logical_key.len()
        )));
    }
    let key_len = logical_key.len() as u16;
    let mut record = Vec::with_capacity(RECORD_HEADER_LEN + logical_key.len() + value.len());
    record.extend_from_slice(RECORD_MAGIC);
    record.extend_from_slice(&key_len.to_le_bytes());
    record.extend_from_slice(logical_key);
    record.extend_from_slice(value);
    Ok(record)
}

fn decode_record(record: &[u8]) -> Result<(&[u8], &[u8])> {
    if record.len() < RECORD_HEADER_LEN || &record[..3] != RECORD_MAGIC {
        return Err(StoreError::Validation(
            "invalid WhatsApp NVS record header".to_string(),
        ));
    }
    let key_len = u16::from_le_bytes([record[3], record[4]]) as usize;
    if key_len > MAX_LOGICAL_KEY_LEN || key_len > record.len() - RECORD_HEADER_LEN {
        return Err(StoreError::Validation(
            "invalid WhatsApp NVS logical-key length".to_string(),
        ));
    }
    let value_offset = RECORD_HEADER_LEN + key_len;
    Ok((
        &record[RECORD_HEADER_LEN..value_offset],
        &record[value_offset..],
    ))
}

/// Fifteen base32 characters retain 75 bits of SHA-256 while fitting NVS's
/// 15-character key limit. The full logical key in the record detects collisions.
fn record_name(logical_key: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let digest = Sha256::digest(logical_key);
    let mut name = String::with_capacity(15);
    for index in 0..15 {
        let bit = index * 5;
        let byte = bit / 8;
        let shift = bit % 8;
        let mut value = digest[byte] << shift;
        if shift > 3 {
            value |= digest[byte + 1] >> (8 - shift);
        }
        name.push(ALPHABET[(value >> 3) as usize] as char);
    }
    name
}

fn decode_string_key(key: Vec<u8>, label: &str) -> anyhow::Result<String> {
    String::from_utf8(key).map_err(|error| anyhow::anyhow!("invalid {label} key: {error}"))
}

fn decode_u32_key(key: &[u8], label: &str) -> anyhow::Result<u32> {
    let bytes: [u8; 4] = key
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid {label} numeric key length: {}", key.len()))?;
    Ok(u32::from_le_bytes(bytes))
}

fn ensure_insert_capacity(exists: bool, len: usize, max: usize, label: &str) -> Result<()> {
    if !exists && len >= max {
        return Err(StoreError::Validation(format!(
            "{label} storage limit reached ({max} records)"
        )));
    }
    Ok(())
}

/// Recover from mutex poisoning (a thread panicked while holding the lock).
/// Logs the event so panics don't go unnoticed.
pub(crate) fn recover_poisoned<T>(
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

/// Default NVS partition name for WhatsApp credentials and Signal state.
pub const DEFAULT_PARTITION_NAME: &str = "wa_store";

impl NvsStore {
    /// Open the WhatsApp store from the default partition (`wa_store`).
    pub fn open_default() -> anyhow::Result<Self> {
        Self::open(DEFAULT_PARTITION_NAME)
    }

    pub fn open(partition_name: &str) -> anyhow::Result<Self> {
        // EspCustomNvsPartition::take() repairs NO_FREE_PAGES/NEW_VERSION by erasing.
        // Credential loss must never be an implicit recovery policy, so preflight
        // initialization and fail before `take()` can enter that branch.
        let partition_name_c = std::ffi::CString::new(partition_name)?;
        let init_result =
            unsafe { esp_idf_svc::sys::nvs_flash_init_partition(partition_name_c.as_ptr()) };
        if let Some(error) = esp_idf_svc::sys::EspError::from(init_result) {
            anyhow::bail!(
                "WhatsApp NVS partition '{partition_name}' cannot be opened without repair: {error}"
            );
        }

        let partition = EspCustomNvsPartition::take(partition_name)?;
        let flash = FlashNamespaces::open(partition)?;
        if flash.reset_pending()? {
            log::warn!("Completing interrupted WhatsApp credential reset");
            flash.complete_reset()?;
        }
        let mut inner = StoreInner {
            device: flash.load_device()?,
            ..Default::default()
        };
        let mut timestamp_latest: Option<Vec<u8>> = None;
        if inner.device.is_some() {
            for (key, value) in
                flash.load_records(&flash.identities, "identity", MAX_IDENTITIES, 32)?
            {
                let address = decode_string_key(key, "identity")?;
                let key: [u8; 32] = value.as_slice().try_into().map_err(|_| {
                    anyhow::anyhow!("identity record for '{address}' has {} bytes", value.len())
                })?;
                if inner.identities.insert(address, key).is_some() {
                    anyhow::bail!("duplicate identity record in WhatsApp NVS");
                }
            }
            for (key, value) in flash.load_records(
                &flash.sessions,
                "session",
                MAX_SESSIONS,
                MAX_SIGNAL_RECORD_LEN,
            )? {
                let address = decode_string_key(key, "session")?;
                if inner.sessions.insert(address, Bytes::from(value)).is_some() {
                    anyhow::bail!("duplicate session record in WhatsApp NVS");
                }
            }
            for (key, value) in
                flash.load_records(&flash.prekeys, "prekey", MAX_PREKEYS, MAX_SIGNAL_RECORD_LEN)?
            {
                let id = decode_u32_key(&key, "prekey")?;
                if inner.prekeys.insert(id, Bytes::from(value)).is_some() {
                    anyhow::bail!("duplicate prekey record in WhatsApp NVS");
                }
            }
            for (key, value) in flash.load_records(
                &flash.signed_prekeys,
                "signed prekey",
                MAX_SIGNED_PREKEYS,
                MAX_SIGNAL_RECORD_LEN,
            )? {
                let id = decode_u32_key(&key, "signed prekey")?;
                if inner.signed_prekeys.insert(id, value).is_some() {
                    anyhow::bail!("duplicate signed-prekey record in WhatsApp NVS");
                }
            }
            for (key, value) in flash.load_records(
                &flash.sender_keys,
                "sender key",
                MAX_SENDER_KEYS,
                MAX_SIGNAL_RECORD_LEN,
            )? {
                let address = decode_string_key(key, "sender key")?;
                if inner.sender_keys.insert(address, value).is_some() {
                    anyhow::bail!("duplicate sender-key record in WhatsApp NVS");
                }
            }
            for (key_id, value) in flash.load_records(
                &flash.sync_keys,
                "sync key",
                MAX_SYNC_KEYS,
                MAX_SIGNAL_RECORD_LEN,
            )? {
                let key: AppStateSyncKey = serde_json::from_slice(&value)
                    .map_err(|error| anyhow::anyhow!("invalid sync key record: {error}"))?;
                // Fallback for a store written before the latest id was recorded:
                // the newest key by the phone's own timestamp is the best guess
                // at the one written last. The recorded id below wins over it.
                let newer = timestamp_latest
                    .as_ref()
                    .and_then(|id| inner.sync_keys.get(id))
                    .is_none_or(|latest: &AppStateSyncKey| key.timestamp >= latest.timestamp);
                if newer {
                    timestamp_latest = Some(key_id.clone());
                }
                if inner.sync_keys.insert(key_id, key).is_some() {
                    anyhow::bail!("duplicate sync-key record in WhatsApp NVS");
                }
            }
            // Only trust a recorded id that names a key actually present. If the
            // marker is unreadable or corrupted, fall back to timestamp_latest.
            let recorded = match flash.load_latest_sync_key_id() {
                Ok(recorded) => recorded,
                Err(error) => {
                    log::warn!("Ignoring unreadable latest sync-key marker: {error}");
                    None
                }
            };
            inner.latest_sync_key_id = recorded
                .filter(|id| inner.sync_keys.contains_key(id))
                .or(timestamp_latest);
        } else if flash.has_signal_records()? {
            log::warn!("Discarding orphaned Signal records without a linked device");
            flash.erase_signal()?;
        }

        log::info!(
            "WhatsApp NVS loaded: device={}, identities={}, sessions={}, prekeys={}, signed_prekeys={}, sender_keys={}, sync_keys={}",
            inner.device.is_some(),
            inner.identities.len(),
            inner.sessions.len(),
            inner.prekeys.len(),
            inner.signed_prekeys.len(),
            inner.sender_keys.len(),
            inner.sync_keys.len(),
        );

        Ok(Self {
            inner: Mutex::new(inner),
            flash: FlashWorker::start(flash)?,
            operation: Mutex::new(()),
            accepting_writes: AtomicBool::new(true),
        })
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

    /// Clear all signal sessions, on flash and in RAM. Returns the count deleted.
    pub fn clear_sessions(&self) -> Result<usize> {
        let _operation = self.operation.lock().unwrap_or_else(recover_poisoned);
        let mut s = self.lock();
        self.flash
            .run(|flash| erase_namespace(&flash.sessions, "session"))?;
        let count = s.sessions.len();
        s.sessions.clear();
        Ok(count)
    }

    /// Full reset: erase the partition's namespaces, then reassign a fresh
    /// default so the RAM mirror matches. The field set lives in exactly one
    /// place (`#[derive(Default)]` on `StoreInner`), so a newly added store
    /// field can never be silently left behind on a factory reset.
    pub fn reset(&self) -> Result<()> {
        let _operation = self.operation.lock().unwrap_or_else(recover_poisoned);
        let mut s = self.lock();
        self.flash.run(FlashNamespaces::reset_all)?;
        *s = StoreInner::default();
        Ok(())
    }

    /// Called only once the client has been asked to stop. Once sealed, stale
    /// background work (a session flush racing the reset) cannot recreate
    /// records before the reboot.
    pub fn seal_writes(&self) {
        let _operation = self.operation.lock().unwrap_or_else(recover_poisoned);
        self.accepting_writes.store(false, Ordering::Release);
    }

    /// Reboot. ESP-IDF disables external-memory access while restarting, so
    /// this has to run on the NVS worker's internal-RAM stack, not on `wa-main`
    /// or the httpd worker, whose stacks are in PSRAM. Only returns on failure.
    pub fn restart(&self) -> Result<()> {
        self.flash.run(|_| {
            std::thread::sleep(Duration::from_millis(200));
            unsafe { esp_idf_svc::sys::esp_restart() }
        })
    }

    fn write_operation(&self) -> Result<std::sync::MutexGuard<'_, ()>> {
        let operation = self.operation.lock().unwrap_or_else(recover_poisoned);
        if !self.accepting_writes.load(Ordering::Acquire) {
            return Err(StoreError::Validation(
                "WhatsApp storage is sealed for maintenance".to_string(),
            ));
        }
        Ok(operation)
    }
}

pub struct StoreStats {
    pub sessions: usize,
    pub identities: usize,
    pub prekeys: usize,
    pub sender_keys: usize,
    pub device_exists: bool,
}

#[async_trait]
impl SignalStore for NvsStore {
    async fn put_identity(&self, address: &str, key: [u8; 32]) -> Result<()> {
        let _operation = self.write_operation()?;
        let mut s = self.lock();
        ensure_insert_capacity(
            s.identities.contains_key(address),
            s.identities.len(),
            MAX_IDENTITIES,
            "identity",
        )?;
        let logical_key = address.as_bytes().to_vec();
        self.flash.run(move |flash| {
            flash.put_record(&flash.identities, &logical_key, &key, "identity")
        })?;
        s.identities.insert(address.to_string(), key);
        Ok(())
    }

    async fn load_identity(&self, address: &str) -> Result<Option<[u8; 32]>> {
        Ok(self.lock().identities.get(address).copied())
    }

    async fn delete_identity(&self, address: &str) -> Result<()> {
        let _operation = self.write_operation()?;
        let mut s = self.lock();
        let logical_key = address.as_bytes().to_vec();
        self.flash
            .run(move |flash| flash.delete_record(&flash.identities, &logical_key, "identity"))?;
        s.identities.remove(address);
        Ok(())
    }

    async fn get_session(&self, address: &str) -> Result<Option<Bytes>> {
        Ok(self.lock().sessions.get(address).cloned())
    }

    async fn put_session(&self, address: &str, session: &[u8]) -> Result<()> {
        let _operation = self.write_operation()?;
        let mut s = self.lock();
        ensure_insert_capacity(
            s.sessions.contains_key(address),
            s.sessions.len(),
            MAX_SESSIONS,
            "session",
        )?;
        let logical_key = address.as_bytes().to_vec();
        let persisted = Bytes::copy_from_slice(session);
        let mirrored = persisted.clone();
        self.flash.run(move |flash| {
            flash.put_record(&flash.sessions, &logical_key, &persisted, "session")
        })?;
        s.sessions.insert(address.to_string(), mirrored);
        Ok(())
    }

    async fn delete_session(&self, address: &str) -> Result<()> {
        let _operation = self.write_operation()?;
        let mut s = self.lock();
        let logical_key = address.as_bytes().to_vec();
        self.flash
            .run(move |flash| flash.delete_record(&flash.sessions, &logical_key, "session"))?;
        s.sessions.remove(address);
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
        let _operation = self.write_operation()?;
        let mut s = self.lock();
        ensure_insert_capacity(
            s.prekeys.contains_key(&id),
            s.prekeys.len(),
            MAX_PREKEYS,
            "prekey",
        )?;
        let persisted = Bytes::copy_from_slice(record);
        let mirrored = persisted.clone();
        self.flash.run(move |flash| {
            flash.put_record(&flash.prekeys, &id.to_le_bytes(), &persisted, "prekey")
        })?;
        s.prekeys.insert(id, mirrored);
        Ok(())
    }

    async fn load_prekey(&self, id: u32) -> Result<Option<Bytes>> {
        Ok(self.lock().prekeys.get(&id).cloned())
    }

    async fn remove_prekey(&self, id: u32) -> Result<()> {
        let _operation = self.write_operation()?;
        let mut s = self.lock();
        self.flash
            .run(move |flash| flash.delete_record(&flash.prekeys, &id.to_le_bytes(), "prekey"))?;
        s.prekeys.remove(&id);
        Ok(())
    }

    async fn get_max_prekey_id(&self) -> Result<u32> {
        Ok(self.lock().prekeys.keys().copied().max().unwrap_or(0))
    }

    async fn mark_prekeys_uploaded(&self, _ids: &[u32]) -> Result<()> {
        Ok(()) // The device's upload watermark tracks this; records need no extra field.
    }

    async fn store_signed_prekey(&self, id: u32, record: &[u8]) -> Result<()> {
        let _operation = self.write_operation()?;
        let mut s = self.lock();
        ensure_insert_capacity(
            s.signed_prekeys.contains_key(&id),
            s.signed_prekeys.len(),
            MAX_SIGNED_PREKEYS,
            "signed prekey",
        )?;
        let persisted = record.to_vec();
        self.flash.run(move |flash| {
            flash.put_record(
                &flash.signed_prekeys,
                &id.to_le_bytes(),
                &persisted,
                "signed prekey",
            )
        })?;
        s.signed_prekeys.insert(id, record.to_vec());
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
        let _operation = self.write_operation()?;
        let mut s = self.lock();
        self.flash.run(move |flash| {
            flash.delete_record(&flash.signed_prekeys, &id.to_le_bytes(), "signed prekey")
        })?;
        s.signed_prekeys.remove(&id);
        Ok(())
    }

    async fn put_sender_key(&self, address: &str, record: &[u8]) -> Result<()> {
        let _operation = self.write_operation()?;
        let mut s = self.lock();
        ensure_insert_capacity(
            s.sender_keys.contains_key(address),
            s.sender_keys.len(),
            MAX_SENDER_KEYS,
            "sender key",
        )?;
        let logical_key = address.as_bytes().to_vec();
        let persisted = record.to_vec();
        self.flash.run(move |flash| {
            flash.put_record(&flash.sender_keys, &logical_key, &persisted, "sender key")
        })?;
        s.sender_keys.insert(address.to_string(), record.to_vec());
        Ok(())
    }

    async fn get_sender_key(&self, address: &str) -> Result<Option<Vec<u8>>> {
        Ok(self.lock().sender_keys.get(address).cloned())
    }

    async fn delete_sender_key(&self, address: &str) -> Result<()> {
        let _operation = self.write_operation()?;
        let mut s = self.lock();
        let logical_key = address.as_bytes().to_vec();
        self.flash.run(move |flash| {
            flash.delete_record(&flash.sender_keys, &logical_key, "sender key")
        })?;
        s.sender_keys.remove(address);
        Ok(())
    }
}

#[async_trait]
impl AppSyncStore for NvsStore {
    async fn get_sync_key(&self, key_id: &[u8]) -> Result<Option<AppStateSyncKey>> {
        Ok(self.lock().sync_keys.get(key_id).cloned())
    }

    async fn set_sync_key(&self, key_id: &[u8], key: AppStateSyncKey) -> Result<()> {
        let _operation = self.write_operation()?;
        let mut s = self.lock();
        ensure_insert_capacity(
            s.sync_keys.contains_key(key_id),
            s.sync_keys.len(),
            MAX_SYNC_KEYS,
            "sync key",
        )?;
        let payload =
            serde_json::to_vec(&key).map_err(|error| StoreError::Serialization(Box::new(error)))?;
        let logical_key = key_id.to_vec();
        let latest = key_id.to_vec();
        self.flash.run(move |flash| {
            // Persist the latest-ID marker first: `open` verifies that the key it
            // names is actually present (`.filter(|id| inner.sync_keys.contains_key(id))`).
            // If power fails before `put_record` commits, the orphan marker is
            // ignored and `open` falls back to `timestamp_latest`. If `put_record`
            // were first, an interrupted write would leave the old marker valid,
            // selecting an outdated sync key on reboot.
            flash.save_latest_sync_key_id(&latest)?;
            flash.put_record(&flash.sync_keys, &logical_key, &payload, "sync key")
        })?;
        s.latest_sync_key_id = Some(key_id.to_vec());
        s.sync_keys.insert(key_id.to_vec(), key);
        Ok(())
    }

    async fn get_version(&self, name: &str) -> Result<Option<HashState>> {
        Ok(self.lock().versions.get(name).cloned())
    }

    async fn delete_version(&self, name: &str) -> Result<()> {
        self.lock().versions.remove(name);
        Ok(())
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
impl ProtocolStore for NvsStore {
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
        self.lock()
            .device_lists
            .insert(Arc::clone(&record.user), record);
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
impl DeviceStore for NvsStore {
    async fn save(&self, device: &Device) -> Result<()> {
        let _operation = self.write_operation()?;
        let mut s = self.lock();
        let persisted = device.clone();
        self.flash.run(move |flash| flash.save_device(&persisted))?;
        s.device = Some(device.clone());
        Ok(())
    }

    async fn load(&self) -> Result<Option<Device>> {
        Ok(self.lock().device.clone())
    }

    async fn exists(&self) -> Result<bool> {
        Ok(self.lock().device.is_some())
    }

    async fn create(&self) -> Result<i32> {
        let _operation = self.write_operation()?;
        let mut s = self.lock();
        s.device_id_counter += 1;
        Ok(s.device_id_counter)
    }
}

#[async_trait]
impl MsgSecretStore for NvsStore {
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
