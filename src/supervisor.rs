//! Shared state between the client supervisor loop, the maintenance path and
//! the optional admin server: which [`Client`] is live, what the dashboard
//! shows, and the one arbiter for every operation that ends in a reboot.
//!
//! None of this is part of the `whatsapp-rust` platform contract. The four
//! platform shims ([`crate::storage`], [`crate::transport`],
//! [`crate::http_client`], [`crate::runtime`]) are all a `Bot` needs; this
//! module is the firmware's own bookkeeping, kept public because the admin
//! server and the demo firmware share it.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use whatsapp_rust::serde_json;
use whatsapp_rust::Client;

use crate::storage::{recover_poisoned, NvsStore};

/// Longest message text kept per entry. The log is a dashboard preview, and the
/// text is whatever a remote sender chose to send, so it is truncated rather
/// than retained whole: a handful of maximum-size messages would otherwise pin
/// megabytes of PSRAM for a panel that shows one line each.
pub const MAX_LOGGED_TEXT_BYTES: usize = 256;

/// How many inbound messages `/messages` keeps for the dashboard and the tests.
const MESSAGE_LOG_CAPACITY: usize = 16;

/// The client the supervisor loop is currently running, so the admin server can
/// send through it and events from a superseded instance can be ignored.
pub struct ActiveClient {
    inner: Mutex<Option<Arc<Client>>>,
}

impl Default for ActiveClient {
    fn default() -> Self {
        Self::new()
    }
}

impl ActiveClient {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }

    pub fn set(&self, client: Arc<Client>) {
        *self.inner.lock().unwrap_or_else(recover_poisoned) = Some(client);
    }

    pub fn current(&self) -> Option<Arc<Client>> {
        self.inner.lock().unwrap_or_else(recover_poisoned).clone()
    }

    pub fn is_current(&self, client: &Arc<Client>) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(recover_poisoned)
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, client))
    }

    pub fn clear_if(&self, client: &Arc<Client>) {
        let mut current = self.inner.lock().unwrap_or_else(recover_poisoned);
        if current
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(active, client))
        {
            *current = None;
        }
    }
}

/// What a maintenance request asks for, ordered by how much it destroys: a
/// request can only be upgraded, never downgraded, while one is in flight.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum MaintenanceAction {
    Reboot,
    ClearSessions,
    Reset,
}

pub enum MaintenanceRequest {
    /// The caller must start the maintenance task.
    Start,
    /// A task is already running and will pick this request up.
    Queued,
    /// The reboot is under way; nothing more can be queued.
    Rejected,
}

#[derive(Default)]
struct MaintenanceState {
    requested: Option<MaintenanceAction>,
    running: bool,
    accepting: bool,
}

/// Arbitrates every operation that ends in a reboot. Requests can only upgrade
/// in priority, so a server-forced credential reset wins over a concurrent
/// session clear or plain reboot, and two dashboard clicks never race two
/// erase-and-reboot sequences against each other.
pub struct MaintenanceCoordinator {
    inner: Mutex<MaintenanceState>,
}

impl Default for MaintenanceCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

impl MaintenanceCoordinator {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(MaintenanceState {
                accepting: true,
                ..Default::default()
            }),
        }
    }

    pub fn request(&self, action: MaintenanceAction) -> MaintenanceRequest {
        let mut state = self.inner.lock().unwrap_or_else(recover_poisoned);
        if !state.accepting {
            return MaintenanceRequest::Rejected;
        }
        state.requested = Some(
            state
                .requested
                .map_or(action, |current| current.max(action)),
        );
        if state.running {
            MaintenanceRequest::Queued
        } else {
            state.running = true;
            MaintenanceRequest::Start
        }
    }

    pub fn requested(&self) -> MaintenanceAction {
        self.inner
            .lock()
            .unwrap_or_else(recover_poisoned)
            .requested
            .unwrap_or(MaintenanceAction::Reboot)
    }

    /// Atomically closes the request window once the highest requested action
    /// has been applied. A concurrent higher-priority request makes the caller
    /// loop and apply it before rebooting.
    pub fn begin_reboot(&self, applied: MaintenanceAction) -> bool {
        let mut state = self.inner.lock().unwrap_or_else(recover_poisoned);
        if state.requested.is_some_and(|requested| requested > applied) {
            return false;
        }
        state.accepting = false;
        true
    }

    pub fn is_idle(&self) -> bool {
        let state = self.inner.lock().unwrap_or_else(recover_poisoned);
        state.accepting && !state.running
    }

    /// The task that `Start` asked for could not be queued; reopen the window.
    pub fn cancel_start(&self) {
        let mut state = self.inner.lock().unwrap_or_else(recover_poisoned);
        state.running = false;
        state.requested = None;
    }
}

/// The one path that erases or reboots, shared by the dashboard actions and
/// the supervisor's response to being unlinked. Stops the live client first
/// (logout if resetting, disconnect if rebooting), applies the highest action
/// requested by the time it gets there, and reboots on the NVS worker's stack.
pub async fn run_maintenance(
    store: Arc<NvsStore>,
    device_status: Arc<DeviceStatus>,
    active_client: Arc<ActiveClient>,
    maintenance: Arc<MaintenanceCoordinator>,
) {
    let initial = maintenance.requested();
    let client = active_client.current();
    let mut logout_attempted = false;
    if let Some(client) = &client {
        match initial {
            MaintenanceAction::Reset => {
                log::info!("Maintenance: logging out before factory reset");
                client.logout().await;
                logout_attempted = true;
            }
            MaintenanceAction::ClearSessions | MaintenanceAction::Reboot => {
                log::info!("Maintenance: disconnecting WhatsApp client");
                client.disconnect().await;
            }
        }
        active_client.clear_if(client);
    }

    let mut applied = MaintenanceAction::Reboot;
    loop {
        let requested = maintenance.requested();
        if requested > applied {
            let succeeded = match requested {
                MaintenanceAction::Reset => {
                    if !logout_attempted {
                        // A reset may upgrade a session-clear/reboot while its
                        // disconnect is in flight. The remote IQ is best-effort
                        // once offline, but local credential removal remains safe.
                        if let Some(client) = &client {
                            log::info!("Maintenance: reset upgrade, attempting logout");
                            client.logout().await;
                        }
                        logout_attempted = true;
                    }
                    device_status.set_logged_out();
                    store.seal_writes();
                    match store.reset() {
                        Ok(()) => {
                            log::info!("Maintenance: persistent WhatsApp data cleared");
                            true
                        }
                        Err(error) => {
                            log::error!("Maintenance: factory reset failed: {error}; retrying");
                            false
                        }
                    }
                }
                MaintenanceAction::ClearSessions => {
                    store.seal_writes();
                    match store.clear_sessions() {
                        Ok(count) => {
                            log::info!("Maintenance: cleared {count} persistent sessions");
                            true
                        }
                        Err(error) => {
                            log::error!("Maintenance: session clear failed: {error}; retrying");
                            false
                        }
                    }
                }
                MaintenanceAction::Reboot => true,
            };
            if succeeded {
                applied = requested;
            } else {
                // A blocking sleep on the executor thread, deliberately: this is
                // only reached when erasing flash has already failed its own
                // retries, the device is on its way to a reboot either way, and
                // there is nothing else worth running in the meantime.
                std::thread::sleep(std::time::Duration::from_secs(1));
                continue;
            }
        }
        if maintenance.begin_reboot(applied) {
            break;
        }
    }

    if let Err(error) = store.restart() {
        log::error!("Maintenance: could not schedule restart: {error}");
        futures::future::pending::<()>().await;
    }
}

/// One inbound message as the dashboard and `/messages` show it.
pub struct MessageLogEntry {
    pub id: String,
    pub chat: String,
    pub sender: String,
    pub text: Option<String>,
    pub timestamp: i64,
    pub from_me: bool,
}

enum PairCodeState {
    Idle,
    Pending { request_id: u32 },
    Ready { code: String, expires_at: Instant },
    Error { message: String },
}

/// Shared state between the event handler, the admin server and the tests.
pub struct DeviceStatus {
    inner: Mutex<DeviceStatusInner>,
}

struct DeviceStatusInner {
    qr_code: Option<String>,
    connected: bool,
    pn: Option<String>,
    lid: Option<String>,
    pair_code: PairCodeState,
    next_pair_code_request: u32,
    messages: VecDeque<MessageLogEntry>,
}

impl Default for DeviceStatus {
    fn default() -> Self {
        Self::new()
    }
}

impl DeviceStatus {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(DeviceStatusInner {
                qr_code: None,
                connected: false,
                pn: None,
                lid: None,
                pair_code: PairCodeState::Idle,
                next_pair_code_request: 1,
                messages: VecDeque::with_capacity(MESSAGE_LOG_CAPACITY),
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

    pub fn clear_qr_code(&self) {
        self.lock().qr_code = None;
    }

    pub fn set_connected(&self, pn: Option<String>, lid: Option<String>) {
        let mut s = self.lock();
        s.qr_code = None;
        s.connected = true;
        s.pn = pn;
        s.lid = lid;
        s.pair_code = PairCodeState::Idle;
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
        s.messages.clear();
    }

    /// Reserve a phone-number pairing attempt. Only one can be pending or
    /// unexpired at a time; a second request would generate a second code the
    /// server then rejects the first one for.
    pub fn begin_pair_code(&self) -> std::result::Result<u32, &'static str> {
        let mut s = self.lock();
        match &s.pair_code {
            PairCodeState::Pending { .. } => return Err("A linking code is being generated"),
            PairCodeState::Ready { expires_at, .. } if *expires_at > Instant::now() => {
                return Err("A linking code is already active");
            }
            _ => {}
        }
        let request_id = s.next_pair_code_request;
        s.next_pair_code_request = s.next_pair_code_request.wrapping_add(1);
        s.pair_code = PairCodeState::Pending { request_id };
        Ok(request_id)
    }

    fn pair_code_request_is(state: &PairCodeState, wanted: u32) -> bool {
        matches!(state, PairCodeState::Pending { request_id } if *request_id == wanted)
    }

    pub fn complete_pair_code(&self, request_id: u32, code: String, valid_for: Duration) {
        let mut s = self.lock();
        if Self::pair_code_request_is(&s.pair_code, request_id) {
            s.pair_code = PairCodeState::Ready {
                code,
                expires_at: Instant::now() + valid_for,
            };
        }
    }

    pub fn fail_pair_code(&self, request_id: u32, message: &str) {
        let mut s = self.lock();
        if Self::pair_code_request_is(&s.pair_code, request_id) {
            s.pair_code = PairCodeState::Error {
                message: message.to_string(),
            };
        }
    }

    pub fn record_message(&self, mut entry: MessageLogEntry) {
        const ELLIPSIS: char = '…';
        // Edition 2021 here, so no let-chain.
        if let Some(text) = entry
            .text
            .as_mut()
            .filter(|t| t.len() > MAX_LOGGED_TEXT_BYTES)
        {
            // Reserve space for the ellipsis so the preview never exceeds MAX_LOGGED_TEXT_BYTES.
            let budget = MAX_LOGGED_TEXT_BYTES.saturating_sub(ELLIPSIS.len_utf8());
            let end = (0..=budget)
                .rev()
                .find(|&i| text.is_char_boundary(i))
                .unwrap_or(0);
            text.truncate(end);
            text.push(ELLIPSIS);
        }
        let mut s = self.lock();
        if s.messages.len() == MESSAGE_LOG_CAPACITY {
            s.messages.pop_front();
        }
        s.messages.push_back(entry);
    }

    #[allow(dead_code)]
    pub fn to_json(&self) -> String {
        self.to_json_authenticated(true)
    }

    pub fn to_json_authenticated(&self, authenticated: bool) -> String {
        let s = self.lock();
        let pair_code = if !authenticated {
            serde_json::json!({ "state": "redacted" })
        } else {
            match &s.pair_code {
                PairCodeState::Idle => serde_json::json!({ "state": "idle" }),
                PairCodeState::Pending { .. } => serde_json::json!({ "state": "pending" }),
                PairCodeState::Ready { code, expires_at } => {
                    let remaining = expires_at.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        serde_json::json!({ "state": "expired" })
                    } else {
                        serde_json::json!({
                            "state": "ready",
                            "code": code,
                            "expires_in_seconds": remaining.as_secs(),
                        })
                    }
                }
                PairCodeState::Error { message } => {
                    serde_json::json!({ "state": "error", "message": message })
                }
            }
        };
        serde_json::json!({
            "qr_code": if authenticated { s.qr_code.as_deref() } else { None },
            "connected": s.connected,
            "pn": s.pn,
            "lid": s.lid,
            "pair_code": pair_code,
        })
        .to_string()
    }

    pub fn messages_json(&self) -> String {
        let s = self.lock();
        let messages: Vec<serde_json::Value> = s
            .messages
            .iter()
            .map(|m| {
                serde_json::json!({
                    "id": m.id,
                    "chat": m.chat,
                    "sender": m.sender,
                    "text": m.text,
                    "timestamp": m.timestamp,
                    "from_me": m.from_me,
                })
            })
            .collect();
        serde_json::json!({
            "count": messages.len(),
            "messages": messages,
        })
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_maintenance_action_ordering() {
        assert!(MaintenanceAction::Reset > MaintenanceAction::ClearSessions);
        assert!(MaintenanceAction::ClearSessions > MaintenanceAction::Reboot);
        assert!(MaintenanceAction::Reset > MaintenanceAction::Reboot);
    }

    #[test]
    fn test_maintenance_coordinator_lifecycle() {
        let coord = MaintenanceCoordinator::new();
        assert!(coord.is_idle());

        // First request is Start
        assert_eq!(
            coord.request(MaintenanceAction::Reboot),
            MaintenanceRequest::Start
        );
        assert!(!coord.is_idle());

        // Upgraded request is Queued
        assert_eq!(
            coord.request(MaintenanceAction::Reset),
            MaintenanceRequest::Queued
        );
        assert_eq!(coord.requested(), MaintenanceAction::Reset);

        // Begin reboot locks in the action
        assert!(coord.begin_reboot(MaintenanceAction::Reset));

        // After commit, new requests are rejected
        assert_eq!(
            coord.request(MaintenanceAction::Reboot),
            MaintenanceRequest::Rejected
        );
    }

    #[test]
    fn test_device_status_state() {
        let status = DeviceStatus::new();
        assert!(!status.is_connected());
        assert!(status.qr_code().is_none());

        status.set_qr_code("test_qr_code_123".to_string());
        assert_eq!(status.qr_code().as_deref(), Some("test_qr_code_123"));

        status.set_connected(
            Some("15551234567".to_string()),
            Some("lid_user".to_string()),
        );
        assert!(status.is_connected());
        assert!(status.qr_code().is_none()); // Connected clears QR code

        status.set_disconnected();
        assert!(!status.is_connected());
    }
}
