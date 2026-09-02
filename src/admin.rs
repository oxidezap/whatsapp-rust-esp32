use embedded_svc::http::server::Connection;
use embedded_svc::http::Headers;
use esp_idf_svc::http::server::{Configuration, EspHttpServer, Request};
use esp_idf_svc::io::Read;
use log::{info, warn};
use std::sync::Arc;
use std::time::Duration;
use whatsapp_rust::pair_code::PairCodeOptions;
use whatsapp_rust::prelude::wa;
use whatsapp_rust::serde_json;
use whatsapp_rust::Jid;

use crate::runtime::BoxedTask;
use crate::storage::{
    ActiveClient, DeviceStatus, MaintenanceAction, MaintenanceCoordinator, MaintenanceRequest,
    NvsStore,
};

const ADMIN_PORT: u16 = 8081;

/// Header carrying the admin token, when one is configured.
const TOKEN_HEADER: &str = "x-admin-token";

/// The token the sensitive routes require, or `None` when the device was
/// flashed without one.
///
/// The dashboard has always been unauthenticated on the LAN, which was already
/// enough to factory-reset the device. Reading recent messages and sending as
/// the linked account is a different kind of exposure, so those routes and the
/// destructive ones can be put behind a shared secret. It is opt-in because a
/// device with no token configured must keep working exactly as before; the
/// boot log says which of the two a given device is.
pub struct AdminAuth {
    token: Option<String>,
}

impl AdminAuth {
    pub fn new(token: Option<String>) -> Self {
        match &token {
            Some(_) => info!("Admin API: token required on the sensitive routes"),
            None => warn!(
                "Admin API: no token configured, so anyone who can reach port {ADMIN_PORT} can read recent messages, send as this account and factory-reset it. Set ADMIN_TOKEN (see README \"Configure\") to require one."
            ),
        }
        Self { token }
    }

    /// `Ok(())` when the request may proceed. Compares in constant time for the
    /// length it does compare: the token is a shared secret, not a hash.
    fn check<C: Connection>(
        &self,
        req: &Request<C>,
    ) -> std::result::Result<(), (u16, &'static str)> {
        let Some(expected) = self.token.as_deref() else {
            return Ok(());
        };
        let provided = req.header(TOKEN_HEADER).unwrap_or_default();
        let matches = provided.len() == expected.len()
            && provided
                .bytes()
                .zip(expected.bytes())
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                == 0;
        if matches {
            Ok(())
        } else {
            Err((
                401,
                r#"{"error":"A valid X-Admin-Token header is required"}"#,
            ))
        }
    }
}

/// How long `POST /send` waits for the executor to report the send's outcome
/// before answering 504. A DM to an established session takes well under a
/// second; a first message to a new contact fetches prekeys first.
///
/// ESP-IDF's httpd serves requests on one task, so a send in flight is also the
/// dashboard not answering. That is the price of reporting the real outcome
/// (and a message id) instead of a 202 the caller then has to poll for, and it
/// is why this waits in seconds rather than minutes.
const SEND_TIMEOUT: Duration = Duration::from_secs(30);

const DASHBOARD_HTML: &str = r#"<!DOCTYPE html>
<html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>ESP32 WhatsApp</title>
<script src="https://cdn.jsdelivr.net/npm/qrcode@1.5.4/build/qrcode.min.js"></script>
<style>
*{box-sizing:border-box;margin:0;padding:0}
body{font-family:system-ui,-apple-system,sans-serif;background:#0a0a0a;color:#e0e0e0;padding:20px;max-width:600px;margin:0 auto}
h1{color:#25D366;margin-bottom:20px;font-size:1.5em}
.card{background:#1a1a1a;border-radius:8px;padding:16px;margin-bottom:12px}
.card h2{font-size:0.9em;color:#888;margin-bottom:8px;text-transform:uppercase;letter-spacing:0.5px}
.stat{font-size:1.6em;font-weight:bold;color:#fff}
.stat small{font-size:0.5em;color:#666;font-weight:normal}
.grid{display:grid;grid-template-columns:1fr 1fr;gap:12px}
button{background:#25D366;color:#fff;border:none;padding:10px 20px;border-radius:6px;cursor:pointer;font-size:0.95em;width:100%;margin-top:8px}
button:hover{background:#1da851}
button:disabled{background:#555;cursor:not-allowed}
button.danger{background:#e53935}
button.danger:hover{background:#c62828}
input{width:100%;background:#111;color:#fff;border:1px solid #444;border-radius:6px;padding:11px;font-size:1em;margin-top:8px}
.dot{display:inline-block;width:10px;height:10px;border-radius:50%;margin-right:6px}
.dot.ok{background:#25D366}.dot.err{background:#e53935}.dot.wait{background:#FFA000}
#qr-section{text-align:center;padding:20px}
#qr-section canvas{margin:10px auto;border-radius:8px}
.pair-code{font-family:monospace;font-size:2em;font-weight:bold;letter-spacing:0.12em;color:#25D366;text-align:center;margin:14px 0}
.hint{color:#888;font-size:0.85em;margin-top:8px}
.device-info{font-family:monospace;font-size:0.85em;color:#aaa;word-break:break-all}
.device-info span{color:#25D366}
#messages{font-family:monospace;font-size:0.8em;color:#aaa;max-height:200px;overflow-y:auto;white-space:pre-wrap}
#messages span{color:#25D366}
#log{background:#111;padding:10px;border-radius:6px;font-family:monospace;font-size:0.8em;max-height:150px;overflow-y:auto;margin-top:12px;white-space:pre-wrap;color:#888}
.hidden{display:none}
</style></head><body>
<h1>&#x1F4F1; ESP32 WhatsApp</h1>

<div id="qr-section" class="card hidden">
<h2>Scan QR Code to Pair</h2>
<div id="qr-canvas"></div>
<p style="color:#888;font-size:0.85em">Open WhatsApp > Linked Devices > Link a Device</p>
</div>

<div id="pair-code-section" class="card hidden">
<h2>Link with phone number</h2>
<form id="pair-code-form" onsubmit="requestPairCode(event)">
<input id="phone-number" type="tel" inputmode="tel" autocomplete="tel" placeholder="International number, e.g. +15551234567" required>
<button id="pair-code-submit" type="submit">Generate linking code</button>
</form>
<div id="pair-code-message" class="hint">Enter the WhatsApp account's phone number.</div>
<div id="pair-code-ready" class="hidden">
<div id="pair-code-value" class="pair-code"></div>
<p id="pair-code-expiry" class="hint"></p>
<p class="hint">WhatsApp &gt; Linked Devices &gt; Link a Device &gt; Link with phone number instead</p>
</div>
</div>

<div id="status-card" class="card">
<h2>Connection</h2>
<div id="status">Loading...</div>
</div>

<div id="device-card" class="card hidden">
<h2>Device Identity</h2>
<div class="device-info" id="device-info"></div>
</div>

<div id="send-card" class="card hidden">
<h2>Send a message</h2>
<form onsubmit="sendMessage(event)">
<input id="send-to" placeholder="Recipient, e.g. 15551234567@s.whatsapp.net" required>
<input id="send-text" placeholder="Text" required>
<button id="send-submit" type="submit">Send</button>
</form>
</div>

<div class="card"><h2>Recent messages</h2><div id="messages">-</div></div>

<div class="grid">
<div class="card"><h2>Free Heap</h2><div class="stat" id="heap">-</div></div>
<div class="card"><h2>Sessions</h2><div class="stat" id="sessions">-</div></div>
<div class="card"><h2>Identities</h2><div class="stat" id="identities">-</div></div>
<div class="card"><h2>Prekeys</h2><div class="stat" id="prekeys">-</div></div>
</div>

<div class="card"><h2>System</h2><div class="device-info" id="sys">-</div></div>
<div class="grid">
<div class="card"><h2>Internal DRAM free</h2><div class="stat" id="dram">-</div></div>
<div class="card"><h2>Internal 8-bit min ever</h2><div class="stat" id="dram_min">-</div></div>
<div class="card"><h2>Largest 8-bit block</h2><div class="stat" id="dram_blk">-</div></div>
<div class="card"><h2>PSRAM free</h2><div class="stat" id="psram">-</div></div>
</div>

<div class="card"><h2>Admin token</h2>
<input id="token" type="password" placeholder="Only if this device was flashed with one" oninput="saveToken()">
<div class="hint">Stored in this browser only. Leave empty if the device has no token.</div>
</div>

<div class="card"><h2>Actions</h2>
<button onclick="if(confirm('Clear all Signal sessions and reboot?'))action('/sessions','DELETE')">Clear Sessions</button>
<button class="danger" onclick="if(confirm('Log out, erase the stored credentials and reboot to re-pair?'))action('/reset','POST')">Factory Reset</button>
<button class="danger" onclick="if(confirm('Reboot device?'))action('/reboot','POST')">Reboot</button>
</div>
<div id="log"></div>

<script>
function tok(){try{return localStorage.getItem('adminToken')||''}catch(e){return ''}}
function saveToken(){try{localStorage.setItem('adminToken',document.getElementById('token').value)}catch(e){}}
// Every request carries the token; the device ignores it when it has none.
function authHeaders(extra){const h=Object.assign({},extra||{});const t=tok();if(t)h['X-Admin-Token']=t;return h}
function get(url){return fetch(url,{headers:authHeaders()})}
function fmt(n){if(n>1048576)return(n/1048576).toFixed(1)+' <small>MB</small>';if(n>1024)return(n/1024).toFixed(0)+' <small>KB</small>';return n+' <small>B</small>'}
function esc(s){return String(s).replace(/[&<>]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;'}[c]))}
function log(m){const el=document.getElementById('log');el.textContent=new Date().toLocaleTimeString()+' '+m+'\n'+el.textContent;el.classList.remove('hidden')}
let lastQr=null;
async function refresh(){
 try{
  const [sr,dr,mr,xr]=await Promise.all([get('/'),get('/device'),get('/metrics'),get('/messages')]);
  const s=await sr.json(), d=await dr.json(), m=await mr.json();
  const x=xr.ok?await xr.json():{count:0,messages:[]};

  // Status
  const dot=d.connected?'ok':d.qr_code?'wait':'err';
  const label=d.connected?'Connected':d.qr_code?'Waiting for QR scan':'Disconnected';
  document.getElementById('status').innerHTML='<span class="dot '+dot+'"></span>'+label;

  // QR code
  const qrSec=document.getElementById('qr-section');
  if(d.qr_code && d.qr_code!==lastQr){
    lastQr=d.qr_code;
    qrSec.classList.remove('hidden');
    document.getElementById('qr-canvas').innerHTML='';
    QRCode.toCanvas(document.createElement('canvas'),d.qr_code,{width:280,margin:2,color:{dark:'#000',light:'#fff'}},function(err,canvas){
      if(!err)document.getElementById('qr-canvas').appendChild(canvas);
    });
  } else if(!d.qr_code){
    qrSec.classList.add('hidden');
    lastQr=null;
  }

  // Phone-number linking code: offered whenever the device is not paired.
  const pc=d.pair_code||{state:'idle'};
  const pcSec=document.getElementById('pair-code-section');
  const pcMsg=document.getElementById('pair-code-message');
  const pcReady=document.getElementById('pair-code-ready');
  const pcSubmit=document.getElementById('pair-code-submit');
  pcSec.classList.toggle('hidden',d.connected||!!s.device_paired);
  pcReady.classList.add('hidden');
  pcSubmit.disabled=pc.state==='pending'||pc.state==='ready';
  if(pc.state==='pending'){
    pcMsg.textContent='Generating a linking code...';
  }else if(pc.state==='ready'){
    pcMsg.textContent='Enter this code on your phone:';
    document.getElementById('pair-code-value').textContent=pc.code;
    document.getElementById('pair-code-expiry').textContent='Expires in about '+pc.expires_in_seconds+' seconds.';
    pcReady.classList.remove('hidden');
  }else if(pc.state==='error'){
    pcMsg.textContent=pc.message;
  }else{
    pcMsg.textContent="Enter the WhatsApp account's phone number.";
  }

  // Device info
  const devCard=document.getElementById('device-card');
  if(d.pn||d.lid){
    devCard.classList.remove('hidden');
    let html='';
    if(d.pn)html+='PN: <span>'+esc(d.pn)+'</span><br>';
    if(d.lid)html+='LID: <span>'+esc(d.lid)+'</span>';
    document.getElementById('device-info').innerHTML=html;
  } else { devCard.classList.add('hidden') }
  document.getElementById('send-card').classList.toggle('hidden',!d.connected);

  // Messages, newest first. 401 means the device wants a token we do not have.
  document.getElementById('messages').innerHTML=xr.status===401?'Enter the admin token to see messages.':x.count?x.messages.slice().reverse().map(m=>
    new Date(m.timestamp*1000).toLocaleTimeString()+' <span>'+esc(m.sender)+'</span> '+(m.text==null?'(no text)':esc(m.text))).join('\n'):'-';

  // Stats
  document.getElementById('heap').innerHTML=fmt(s.heap_free);
  document.getElementById('sessions').textContent=s.sessions;
  document.getElementById('identities').textContent=s.identities;
  document.getElementById('prekeys').textContent=s.prekeys;

  // System telemetry
  document.getElementById('dram').innerHTML=fmt(m.heap_internal_free);
  document.getElementById('dram_min').innerHTML=fmt(m.internal_8bit_min_free);
  document.getElementById('dram_blk').innerHTML=fmt(m.internal_8bit_largest_block);
  document.getElementById('psram').innerHTML=fmt(m.psram_free);
  const up=m.uptime_s, upStr=up>3600?(up/3600).toFixed(1)+'h':up>60?(up/60).toFixed(0)+'m':up+'s';
  const rssi=m.rssi_dbm==null?'n/a':m.rssi_dbm+' dBm';
  const reset=m.reset_reason||'?';
  const crashy=/Panic|Watchdog|Brownout/.test(reset);
  const swm=v=>v==null?'n/a':fmt(v);
  let crashHtml='';
  if(m.last_panic) crashHtml+='<br>Last panic: <span style="color:#e53935">'+esc(m.last_panic)+'</span>';
  if(m.coredump){const c=m.coredump;
    crashHtml+='<br>Core dump &mdash; task <span>'+esc(c.task)+'</span> pc <span>'+c.exc_pc+'</span> cause <span>'+c.exc_cause+'</span> addr <span>'+c.fault_addr+'</span>'+
      '<br>Backtrace: <span style="font-size:.85em">'+(c.backtrace||[]).join(' ')+(c.bt_corrupted?' (corrupt)':'')+'</span>';}
  document.getElementById('sys').innerHTML=
    'Uptime: <span>'+upStr+'</span> &nbsp; WiFi: <span>'+rssi+'</span><br>'+
    'PSRAM largest block: <span>'+fmt(m.psram_largest_block)+'</span><br>'+
    'Stack free min &mdash; wa-main: <span>'+swm(m.stack_wa_main_min)+'</span> &nbsp; wa-blocking: <span>'+swm(m.stack_wa_blocking_min)+'</span> &nbsp; wa-nvs: <span>'+swm(m.stack_wa_nvs_min)+'</span> &nbsp; ws-transport: <span>'+swm(m.stack_ws_transport_min)+'</span><br>'+
    'Last reset: <span style="color:'+(crashy?'#e53935':'#25D366')+'">'+reset+'</span>'+crashHtml;
 }catch(e){
  document.getElementById('status').innerHTML='<span class="dot err"></span>Offline';
 }
}
async function action(url,method){
 try{const r=await fetch(url,{method,headers:authHeaders()});const d=await r.json();log(JSON.stringify(d));refresh()}
 catch(e){log('Error: '+e)}
}
async function postJson(url,body){
 const r=await fetch(url,{method:'POST',headers:authHeaders({'Content-Type':'application/json'}),body:JSON.stringify(body)});
 const d=await r.json();
 if(!r.ok)throw new Error(d.error||('HTTP '+r.status));
 return d;
}
async function requestPairCode(event){
 event.preventDefault();
 const button=document.getElementById('pair-code-submit');
 button.disabled=true;
 try{
  await postJson('/pair-code',{phone_number:document.getElementById('phone-number').value});
  document.getElementById('pair-code-message').textContent='Generating a linking code...';
  await refresh();
 }catch(e){
  document.getElementById('pair-code-message').textContent='Error: '+e.message;
  button.disabled=false;
 }
}
async function sendMessage(event){
 event.preventDefault();
 const button=document.getElementById('send-submit');
 button.disabled=true;
 try{
  const d=await postJson('/send',{to:document.getElementById('send-to').value,text:document.getElementById('send-text').value});
  log('sent '+d.message_id+' to '+d.to);
  document.getElementById('send-text').value='';
 }catch(e){log('Send error: '+e.message)}
 button.disabled=false;
}
try{document.getElementById('token').value=tok()}catch(e){}
refresh();setInterval(refresh,3000);
</script></body></html>"#;

type AdminRequest<'a, 'b> = Request<&'a mut esp_idf_svc::http::server::EspHttpConnection<'b>>;

fn json_response(req: AdminRequest<'_, '_>, body: &str) -> anyhow::Result<()> {
    json_response_status(req, 200, body)
}

fn json_response_status(req: AdminRequest<'_, '_>, status: u16, body: &str) -> anyhow::Result<()> {
    let mut resp = req.into_response(
        status,
        None,
        &[
            ("Content-Type", "application/json"),
            ("Cache-Control", "no-store"),
        ],
    )?;
    resp.write(body.as_bytes())?;
    Ok(())
}

fn error_body(message: &str) -> String {
    serde_json::json!({ "error": message }).to_string()
}

/// Read and parse a small JSON request body. Errors carry the status and body
/// to answer with, so a handler can `return json_response_status(req, ..)`.
fn read_json_body<C: Connection>(
    req: &mut Request<C>,
    max_len: usize,
) -> std::result::Result<serde_json::Value, (u16, &'static str)> {
    if !req
        .header("content-type")
        .is_some_and(|value| value.starts_with("application/json"))
    {
        return Err((415, r#"{"error":"Content-Type must be application/json"}"#));
    }
    // `content_len()` is a u64 and usize is 32 bits here, so a cast would wrap a
    // huge declared length down into the accepted range and then short-read.
    let Some(content_len) = req.content_len().and_then(|len| usize::try_from(len).ok()) else {
        return Err((411, r#"{"error":"Content-Length required"}"#));
    };
    if content_len == 0 || content_len > max_len {
        return Err((413, r#"{"error":"Invalid request size"}"#));
    }
    let mut body = vec![0_u8; content_len];
    if req.read_exact(&mut body).is_err() {
        return Err((400, r#"{"error":"Could not read the request body"}"#));
    }
    serde_json::from_slice(&body).map_err(|_| (400, r#"{"error":"Invalid JSON"}"#))
}

/// Queue a reboot-ending maintenance action on the executor. Shared by the
/// three endpoints that end in a reboot, so they cannot race each other: the
/// coordinator upgrades a queued request instead of starting a second task.
#[allow(clippy::too_many_arguments)]
fn queue_maintenance(
    req: AdminRequest<'_, '_>,
    action: MaintenanceAction,
    accepted: &str,
    auth: &AdminAuth,
    store: &Arc<NvsStore>,
    device_status: &Arc<DeviceStatus>,
    active_client: &Arc<ActiveClient>,
    maintenance: &Arc<MaintenanceCoordinator>,
    task_tx: &whatsapp_rust::async_channel::Sender<BoxedTask>,
) -> anyhow::Result<()> {
    if let Err((status, body)) = auth.check(&req) {
        return json_response_status(req, status, body);
    }
    match maintenance.request(action) {
        MaintenanceRequest::Rejected => {
            return json_response_status(req, 409, r#"{"error":"Device is already rebooting"}"#);
        }
        MaintenanceRequest::Queued => {}
        MaintenanceRequest::Start => {
            let task = Box::pin(crate::run_maintenance(
                store.clone(),
                device_status.clone(),
                active_client.clone(),
                maintenance.clone(),
            ));
            if task_tx.try_send(task).is_err() {
                maintenance.cancel_start();
                return json_response_status(
                    req,
                    503,
                    r#"{"error":"WhatsApp executor is unavailable"}"#,
                );
            }
        }
    }
    info!("Admin: {action:?} queued");
    json_response_status(req, 202, accepted)
}

pub fn start_admin_server(
    store: Arc<NvsStore>,
    device_status: Arc<DeviceStatus>,
    active_client: Arc<ActiveClient>,
    maintenance: Arc<MaintenanceCoordinator>,
    task_tx: whatsapp_rust::async_channel::Sender<BoxedTask>,
    auth: Arc<AdminAuth>,
) -> anyhow::Result<EspHttpServer<'static>> {
    let config = Configuration {
        http_port: ADMIN_PORT,
        // Put the httpd worker stack in PSRAM (Default is internal DRAM). Handlers
        // serve static HTML + small JSON and hand real work to the executor, so
        // 6 KB from PSRAM is plenty; JSON parsing of a POST body is the deepest.
        stack_size: 6144,
        task_caps: esp_idf_svc::sys::MALLOC_CAP_SPIRAM | esp_idf_svc::sys::MALLOC_CAP_8BIT,
        ..Default::default()
    };

    let mut server = EspHttpServer::new(&config)?;

    // GET /dashboard
    server.fn_handler::<anyhow::Error, _>("/dashboard", esp_idf_svc::http::Method::Get, |req| {
        let mut resp = req.into_response(200, None, &[("Content-Type", "text/html")])?;
        resp.write(DASHBOARD_HTML.as_bytes())?;
        Ok(())
    })?;

    // GET /: JSON store stats
    {
        let store = store.clone();
        server.fn_handler::<anyhow::Error, _>("/", esp_idf_svc::http::Method::Get, move |req| {
            let heap_free = unsafe { esp_idf_svc::sys::esp_get_free_heap_size() };
            let heap_internal = unsafe { esp_idf_svc::sys::esp_get_free_internal_heap_size() };
            let stats = store.stats();
            let body = serde_json::json!({
                "status": "running",
                "heap_free": heap_free,
                "heap_internal": heap_internal,
                "sessions": stats.sessions,
                "identities": stats.identities,
                "prekeys": stats.prekeys,
                "sender_keys": stats.sender_keys,
                "device_paired": stats.device_exists,
            });
            json_response(req, &body.to_string())
        })?;
    }

    // GET /device: Device status (QR code, connection, PN/LID, pairing code)
    {
        let ds = device_status.clone();
        server.fn_handler::<anyhow::Error, _>(
            "/device",
            esp_idf_svc::http::Method::Get,
            move |req| json_response(req, &ds.to_json()),
        )?;
    }

    // GET /messages: the last inbound messages, oldest first
    {
        let auth = auth.clone();
        let ds = device_status.clone();
        server.fn_handler::<anyhow::Error, _>(
            "/messages",
            esp_idf_svc::http::Method::Get,
            move |req| match auth.check(&req) {
                Ok(()) => json_response(req, &ds.messages_json()),
                Err((status, body)) => json_response_status(req, status, body),
            },
        )?;
    }

    // POST /send {"to": "<jid>", "text": "..."}: send a text message through the
    // live client and wait for the outcome. The handler thread blocks on a
    // channel the executor task answers on; the send itself runs where every
    // other send runs, so nothing here touches the client off its executor.
    {
        let auth = auth.clone();
        let active_client = active_client.clone();
        let task_tx = task_tx.clone();
        server.fn_handler::<anyhow::Error, _>(
            "/send",
            esp_idf_svc::http::Method::Post,
            move |mut req| {
                if let Err((status, body)) = auth.check(&req) {
                    return json_response_status(req, status, body);
                }
                let value = match read_json_body(&mut req, 2048) {
                    Ok(value) => value,
                    Err((status, body)) => return json_response_status(req, status, body),
                };
                let Some(to) = value.get("to").and_then(|value| value.as_str()) else {
                    return json_response_status(req, 400, r#"{"error":"to is required"}"#);
                };
                let Some(text) = value.get("text").and_then(|value| value.as_str()) else {
                    return json_response_status(req, 400, r#"{"error":"text is required"}"#);
                };
                if text.is_empty() {
                    return json_response_status(req, 400, r#"{"error":"text is empty"}"#);
                }
                let Ok(jid) = to.parse::<Jid>() else {
                    return json_response_status(req, 400, r#"{"error":"to is not a JID"}"#);
                };
                let Some(client) = active_client.current() else {
                    return json_response_status(
                        req,
                        503,
                        r#"{"error":"WhatsApp client is starting"}"#,
                    );
                };
                if !client.is_logged_in() {
                    return json_response_status(req, 503, r#"{"error":"Not connected"}"#);
                }

                let message = wa::Message {
                    conversation: Some(text.to_string()),
                    ..Default::default()
                };
                let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
                let task_active_client = active_client.clone();
                let task = Box::pin(async move {
                    // A reconnect between the check above and this task running
                    // replaces the client; sending through the old one would go
                    // out on a socket that is already gone.
                    let result = if task_active_client.is_current(&client) {
                        client
                            .send_message(jid, message)
                            .await
                            .map(|sent| sent.message_id)
                            .map_err(|error| error.to_string())
                    } else {
                        Err("WhatsApp client reconnected; try again".to_string())
                    };
                    let _ = result_tx.send(result);
                });
                if task_tx.try_send(task).is_err() {
                    return json_response_status(
                        req,
                        503,
                        r#"{"error":"WhatsApp executor is unavailable"}"#,
                    );
                }
                match result_rx.recv_timeout(SEND_TIMEOUT) {
                    Ok(Ok(message_id)) => {
                        info!("Admin: sent {message_id} to {to}");
                        let body = serde_json::json!({
                            "result": "sent",
                            "message_id": message_id,
                            "to": to,
                        });
                        json_response(req, &body.to_string())
                    }
                    Ok(Err(error)) => json_response_status(req, 502, &error_body(&error)),
                    Err(_) => json_response_status(
                        req,
                        504,
                        r#"{"error":"Send did not complete in time"}"#,
                    ),
                }
            },
        )?;
    }

    // POST /pair-code {"phone_number": "+15551234567"}: link by phone number
    // instead of scanning a QR. The code is generated on the executor and shows
    // up in /device once ready, since the request itself must not wait 30 s.
    {
        let store = store.clone();
        let ds = device_status.clone();
        let active_client = active_client.clone();
        let maintenance = maintenance.clone();
        let task_tx = task_tx.clone();
        let auth = auth.clone();
        server.fn_handler::<anyhow::Error, _>(
            "/pair-code",
            esp_idf_svc::http::Method::Post,
            move |mut req| {
                if let Err((status, body)) = auth.check(&req) {
                    return json_response_status(req, status, body);
                }
                let value = match read_json_body(&mut req, 128) {
                    Ok(value) => value,
                    Err((status, body)) => return json_response_status(req, status, body),
                };
                let Some(raw_phone) = value.get("phone_number").and_then(|value| value.as_str())
                else {
                    return json_response_status(
                        req,
                        400,
                        r#"{"error":"phone_number is required"}"#,
                    );
                };
                if !raw_phone
                    .chars()
                    .all(|c| c.is_ascii_digit() || matches!(c, '+' | ' ' | '-' | '(' | ')'))
                {
                    return json_response_status(req, 400, r#"{"error":"Invalid phone number"}"#);
                }
                let phone_number: String = raw_phone.chars().filter(char::is_ascii_digit).collect();
                if !(7..=15).contains(&phone_number.len()) || phone_number.starts_with('0') {
                    return json_response_status(
                        req,
                        400,
                        r#"{"error":"Use 7-15 international digits without a leading zero"}"#,
                    );
                }

                if !maintenance.is_idle() {
                    return json_response_status(
                        req,
                        409,
                        r#"{"error":"Device maintenance is in progress"}"#,
                    );
                }
                let Some(client) = active_client.current() else {
                    return json_response_status(
                        req,
                        503,
                        r#"{"error":"WhatsApp client is starting"}"#,
                    );
                };
                if store.stats().device_exists || client.is_logged_in() {
                    return json_response_status(
                        req,
                        409,
                        r#"{"error":"Device is already paired"}"#,
                    );
                }
                let request_id = match ds.begin_pair_code() {
                    Ok(request_id) => request_id,
                    Err(message) => return json_response_status(req, 409, &error_body(message)),
                };

                let task_ds = ds.clone();
                let task_store = store.clone();
                let task_active_client = active_client.clone();
                let task_maintenance = maintenance.clone();
                let task = Box::pin(async move {
                    if client
                        .wait_for_socket(Duration::from_secs(30))
                        .await
                        .is_err()
                    {
                        task_ds.fail_pair_code(request_id, "WhatsApp connection is not ready");
                        return;
                    }
                    if !task_active_client.is_current(&client) {
                        task_ds.fail_pair_code(request_id, "WhatsApp client restarted; try again");
                        return;
                    }
                    if task_store.stats().device_exists || client.is_logged_in() {
                        task_ds.fail_pair_code(request_id, "Device is already paired");
                        return;
                    }
                    // A reset can be requested between the check in the handler
                    // and this task running; pairing into a store that is about
                    // to be erased would strand a code the user cannot use.
                    if !task_maintenance.is_idle() {
                        task_ds.fail_pair_code(request_id, "Device maintenance is in progress");
                        return;
                    }
                    match client
                        .pair_with_code(PairCodeOptions {
                            phone_number,
                            ..Default::default()
                        })
                        .await
                    {
                        Ok(code) => {
                            task_ds.complete_pair_code(request_id, code, Duration::from_secs(180))
                        }
                        Err(error) => {
                            log::warn!("pair-code failed: {error}");
                            task_ds.fail_pair_code(
                                request_id,
                                "Could not generate a pairing code; try again later",
                            )
                        }
                    }
                });
                if task_tx.try_send(task).is_err() {
                    ds.fail_pair_code(request_id, "WhatsApp executor is unavailable");
                    return json_response_status(
                        req,
                        503,
                        r#"{"error":"WhatsApp executor is unavailable"}"#,
                    );
                }
                json_response_status(req, 202, r#"{"result":"pending"}"#)
            },
        )?;
    }

    // GET /health
    server.fn_handler::<anyhow::Error, _>("/health", esp_idf_svc::http::Method::Get, |req| {
        req.into_ok_response()?.write(b"ok")?;
        Ok(())
    })?;

    // POST /test-panic: deliberately crash to exercise the persistent crash
    // diagnostics (RTC panic message + core dump). After the reboot, GET /metrics
    // shows last_panic + the coredump backtrace + reset_reason=Panic.
    server.fn_handler::<anyhow::Error, _>(
        "/test-panic",
        esp_idf_svc::http::Method::Post,
        |_req| panic!("intentional /test-panic: exercises persistent crash capture"),
    )?;

    // GET /metrics: live ESP32 system telemetry (heap/DRAM/PSRAM/uptime/RSSI/reset)
    server.fn_handler::<anyhow::Error, _>(
        "/metrics",
        esp_idf_svc::http::Method::Get,
        move |req| json_response(req, &crate::metrics::system_metrics_json()),
    )?;

    // POST /reset: log out, erase the stored credentials, reboot to re-pair.
    {
        let store = store.clone();
        let device_status = device_status.clone();
        let active_client = active_client.clone();
        let maintenance = maintenance.clone();
        let task_tx = task_tx.clone();
        let auth = auth.clone();
        server.fn_handler::<anyhow::Error, _>(
            "/reset",
            esp_idf_svc::http::Method::Post,
            move |req| {
                queue_maintenance(
                    req,
                    MaintenanceAction::Reset,
                    r#"{"result":"resetting","message":"Logging out, clearing persistent data, and rebooting."}"#,
                    &auth,
                    &store,
                    &device_status,
                    &active_client,
                    &maintenance,
                    &task_tx,
                )
            },
        )?;
    }

    // POST /reboot: disconnect cleanly, then restart.
    {
        let store = store.clone();
        let device_status = device_status.clone();
        let active_client = active_client.clone();
        let maintenance = maintenance.clone();
        let task_tx = task_tx.clone();
        let auth = auth.clone();
        server.fn_handler::<anyhow::Error, _>(
            "/reboot",
            esp_idf_svc::http::Method::Post,
            move |req| {
                queue_maintenance(
                    req,
                    MaintenanceAction::Reboot,
                    r#"{"result":"rebooting"}"#,
                    &auth,
                    &store,
                    &device_status,
                    &active_client,
                    &maintenance,
                    &task_tx,
                )
            },
        )?;
    }

    // GET /sessions
    {
        let store = store.clone();
        server.fn_handler::<anyhow::Error, _>(
            "/sessions",
            esp_idf_svc::http::Method::Get,
            move |req| {
                let sessions = store.list_sessions();
                let body = serde_json::json!({
                    "count": sessions.len(),
                    "addresses": sessions,
                });
                json_response(req, &body.to_string())
            },
        )?;
    }

    // DELETE /sessions: disconnect, erase the Signal sessions, reboot. The
    // sessions are erased with the client offline: clearing them under a live
    // ratchet would leave in-flight encrypts pointing at state that is gone.
    {
        let store = store.clone();
        let device_status = device_status.clone();
        let active_client = active_client.clone();
        let maintenance = maintenance.clone();
        let task_tx = task_tx.clone();
        server.fn_handler::<anyhow::Error, _>(
            "/sessions",
            esp_idf_svc::http::Method::Delete,
            move |req| {
                queue_maintenance(
                    req,
                    MaintenanceAction::ClearSessions,
                    r#"{"result":"clearing","message":"Disconnecting, clearing sessions, and rebooting."}"#,
                    &auth,
                    &store,
                    &device_status,
                    &active_client,
                    &maintenance,
                    &task_tx,
                )
            },
        )?;
    }

    info!("Admin server ready on port {}", ADMIN_PORT);
    Ok(server)
}
