use esp_idf_svc::http::server::{Configuration, EspHttpServer};
use log::info;
use std::sync::Arc;
use whatsapp_rust::serde_json;

const ADMIN_PORT: u16 = 8081;

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
button.danger{background:#e53935}
button.danger:hover{background:#c62828}
.dot{display:inline-block;width:10px;height:10px;border-radius:50%;margin-right:6px}
.dot.ok{background:#25D366}.dot.err{background:#e53935}.dot.wait{background:#FFA000}
#qr-section{text-align:center;padding:20px}
#qr-section canvas{margin:10px auto;border-radius:8px}
.device-info{font-family:monospace;font-size:0.85em;color:#aaa;word-break:break-all}
.device-info span{color:#25D366}
#log{background:#111;padding:10px;border-radius:6px;font-family:monospace;font-size:0.8em;max-height:150px;overflow-y:auto;margin-top:12px;white-space:pre-wrap;color:#888}
.hidden{display:none}
</style></head><body>
<h1>&#x1F4F1; ESP32 WhatsApp</h1>

<div id="qr-section" class="card hidden">
<h2>Scan QR Code to Pair</h2>
<div id="qr-canvas"></div>
<p style="color:#888;font-size:0.85em">Open WhatsApp > Linked Devices > Link a Device</p>
</div>

<div id="status-card" class="card">
<h2>Connection</h2>
<div id="status">Loading...</div>
</div>

<div id="device-card" class="card hidden">
<h2>Device Identity</h2>
<div class="device-info" id="device-info"></div>
</div>

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

<div class="card"><h2>Actions</h2>
<button onclick="action('/sessions','DELETE')">Clear Sessions</button>
<button class="danger" onclick="if(confirm('Reset all data and re-pair?'))action('/reset','POST')">Factory Reset</button>
<button class="danger" onclick="if(confirm('Reboot device?'))action('/reboot','POST')">Reboot</button>
</div>
<div id="log"></div>

<script>
function fmt(n){if(n>1048576)return(n/1048576).toFixed(1)+' <small>MB</small>';if(n>1024)return(n/1024).toFixed(0)+' <small>KB</small>';return n+' <small>B</small>'}
function log(m){const el=document.getElementById('log');el.textContent=new Date().toLocaleTimeString()+' '+m+'\n'+el.textContent;el.classList.remove('hidden')}
let lastQr=null;
async function refresh(){
 try{
  const [sr,dr,mr]=await Promise.all([fetch('/'),fetch('/device'),fetch('/metrics')]);
  const s=await sr.json(), d=await dr.json(), m=await mr.json();

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

  // Device info
  const devCard=document.getElementById('device-card');
  if(d.pn||d.lid){
    devCard.classList.remove('hidden');
    let html='';
    if(d.pn)html+='PN: <span>'+d.pn+'</span><br>';
    if(d.lid)html+='LID: <span>'+d.lid+'</span>';
    document.getElementById('device-info').innerHTML=html;
  } else { devCard.classList.add('hidden') }

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
  if(m.last_panic) crashHtml+='<br>Last panic: <span style="color:#e53935">'+m.last_panic+'</span>';
  if(m.coredump){const c=m.coredump;
    crashHtml+='<br>Core dump &mdash; task <span>'+c.task+'</span> pc <span>'+c.exc_pc+'</span> cause <span>'+c.exc_cause+'</span> addr <span>'+c.fault_addr+'</span>'+
      '<br>Backtrace: <span style="font-size:.85em">'+(c.backtrace||[]).join(' ')+(c.bt_corrupted?' (corrupt)':'')+'</span>';}
  document.getElementById('sys').innerHTML=
    'Uptime: <span>'+upStr+'</span> &nbsp; WiFi: <span>'+rssi+'</span><br>'+
    'PSRAM largest block: <span>'+fmt(m.psram_largest_block)+'</span><br>'+
    'Stack free min &mdash; wa-main: <span>'+swm(m.stack_wa_main_min)+'</span> &nbsp; ws-transport: <span>'+swm(m.stack_ws_transport_min)+'</span><br>'+
    'Last reset: <span style="color:'+(crashy?'#e53935':'#25D366')+'">'+reset+'</span>'+crashHtml;
 }catch(e){
  document.getElementById('status').innerHTML='<span class="dot err"></span>Offline';
 }
}
async function action(url,method){
 try{const r=await fetch(url,{method});const d=await r.json();log(JSON.stringify(d));refresh()}
 catch(e){log('Error: '+e)}
}
refresh();setInterval(refresh,3000);
</script></body></html>"#;

fn json_response(
    req: esp_idf_svc::http::server::Request<&mut esp_idf_svc::http::server::EspHttpConnection>,
    body: &str,
) -> anyhow::Result<()> {
    let mut resp = req.into_response(200, None, &[("Content-Type", "application/json")])?;
    resp.write(body.as_bytes())?;
    Ok(())
}

pub fn start_admin_server(
    store: Arc<crate::storage::MemoryStore>,
    device_status: Arc<crate::storage::DeviceStatus>,
) -> anyhow::Result<EspHttpServer<'static>> {
    let config = Configuration {
        http_port: ADMIN_PORT,
        // Put the httpd worker stack in PSRAM (Default is internal DRAM). Handlers
        // only serve static HTML + small JSON, so 4 KB from PSRAM is plenty.
        stack_size: 4096,
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

    // GET /device: Device status (QR code, connection, PN/LID)
    {
        let ds = device_status.clone();
        server.fn_handler::<anyhow::Error, _>(
            "/device",
            esp_idf_svc::http::Method::Get,
            move |req| json_response(req, &ds.to_json()),
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

    // POST /reset
    {
        let store = store.clone();
        server.fn_handler::<anyhow::Error, _>(
            "/reset",
            esp_idf_svc::http::Method::Post,
            move |req| {
                info!("Admin: resetting all data");
                store.reset();
                json_response(
                    req,
                    r#"{"result":"reset","message":"All data cleared. Reboot to re-pair."}"#,
                )
            },
        )?;
    }

    // POST /reboot
    server.fn_handler::<anyhow::Error, _>("/reboot", esp_idf_svc::http::Method::Post, |req| {
        info!("Admin: rebooting");
        json_response(req, r#"{"result":"rebooting"}"#)?;
        std::thread::sleep(std::time::Duration::from_millis(200));
        unsafe { esp_idf_svc::sys::esp_restart() };
        #[allow(unreachable_code)]
        Ok(())
    })?;

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

    // DELETE /sessions
    {
        let store = store.clone();
        server.fn_handler::<anyhow::Error, _>(
            "/sessions",
            esp_idf_svc::http::Method::Delete,
            move |req| {
                let count = store.clear_sessions();
                info!("Admin: cleared {} sessions", count);
                let body = serde_json::json!({ "result": "cleared", "count": count });
                json_response(req, &body.to_string())
            },
        )?;
    }

    info!("Admin server ready on port {}", ADMIN_PORT);
    Ok(server)
}
