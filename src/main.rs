#![allow(dead_code, unused)]

use anyhow::{anyhow, Context, Result};
use dotenvy::dotenv;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use std::env;
use tokio::time::{sleep, Duration};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use tokio::net::TcpStream;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use std::{io::{self, Write}, thread, time::Instant};
type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Simbol Volatility 10 (bukan 1s). Jika ingin 1s gunakan "1HZ10V".
const SYMBOL: &str = "R_10";

/// Kontrak Fall (Down) pada Rise/Fall = PUT.
const CONTRACT_TYPE_FALL: &str = "PUT";
const CONTRACT_TYPE_RISE: &str = "CALL";

/// Durasi dalam menit.
const DURATION_MINUTES: u32 = 1;

/// Stake default demo & real step-1.
const STAKE_DEMO: f64 = 0.5;
const STAKE_REAL_STEP1: f64 = 0.5;
const STAKE_REAL_STEP2: f64 = 0.79;

static mut RISE: bool = false;

/// Mode eksekusi bot.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    Demo,
    Real,
}

#[derive(Debug, Clone)]
struct AppCfg {
    demo_app_id: String,
    real_app_id: String,
    real_api_token: String,
    demo_api_token: String,
    currency: String,
}

#[derive(Debug)]
struct WsClient {
    write: futures::stream::SplitSink<WsStream, Message>,
    read: futures::stream::SplitStream<WsStream>,
    req_id: u64,
    current_mode: Mode,
}

impl WsClient {
    fn next_req_id(&mut self) -> u64 {
        self.req_id += 1;
        self.req_id
    }
}

#[derive(Debug, Deserialize)]
struct BuyResponse {
    #[serde(rename = "msg_type")]
    msg_type: String,
    buy: Option<BuyInner>,
    error: Option<DerivError>,
}

#[derive(Debug, Deserialize)]
struct BuyInner {
    #[serde(rename = "contract_id")]
    contract_id: Option<u64>,
    #[serde(rename = "longcode")]
    longcode: Option<String>,
    #[serde(rename = "buy_price")]
    buy_price: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct DerivError {
    code: Option<String>,
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProposalResp {
    #[serde(rename = "msg_type")]
    msg_type: String,
    proposal: Option<ProposalInner>,
    error: Option<DerivError>,
}

#[derive(Debug, Deserialize)]
struct ProposalInner {
    id: Option<String>,
    #[serde(rename = "ask_price")]
    ask_price: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct PocResp {
    #[serde(rename = "msg_type")]
    msg_type: String,
    #[serde(rename = "proposal_open_contract")]
    poc: Option<PocInner>,
    error: Option<DerivError>,
}

#[derive(Debug, Deserialize)]
struct PocInner {
    #[serde(rename = "contract_id")]
    contract_id: Option<u64>,
    #[serde(rename = "is_sold")]
    is_sold: Option<i64>,
    #[serde(rename = "buy_price")]
    buy_price: Option<f64>,
    #[serde(rename = "sell_price")]
    sell_price: Option<f64>,
    #[serde(rename = "status")]
    status: Option<String>,
    #[serde(rename = "currency")]
    currency: Option<String>,
    #[serde(rename = "payout")]
    payout: Option<f64>,
    #[serde(rename = "profit")]
    profit: Option<f64>,
}

fn wait_for_minutes(minutes: u64) {
    let start = Instant::now();
    let total = Duration::from_secs(minutes * 60);

    println!("Mulai proses selama {minutes} menit...");

    while start.elapsed() < total {
        print!(".");
        io::stdout().flush().unwrap();
        thread::sleep(Duration::from_secs(1)); // jeda 1 detik
    }

    println!("\nSelesai setelah {} menit!", minutes);
}

/// Membuat koneksi WS dan authorize.
async fn connect_and_auth(app_id: &str, token: &str, mode: Mode) -> Result<WsClient> {
    let url = format!("wss://ws.derivws.com/websockets/v3?app_id={}", app_id);
    info!("📡 Connecting to {:?} mode: {}", mode, url);
    
    let (ws_stream, _) = connect_async(&url).await.with_context(|| "WS connect failed")?;
    let (mut write, mut read) = ws_stream.split();

    // Authorize
    let req = json!({"authorize": token});
    write.send(Message::Text(req.to_string())).await?;
    
    // tunggu ack authorize dengan timeout
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut authorized = false;
    
    loop {
        tokio::select! {
            maybe_msg = read.next() => {
                if let Some(msg) = maybe_msg {
                    let txt = msg?.to_text()?.to_string();
                    let v: Value = serde_json::from_str(&txt).unwrap_or(json!({}));
                    
                    if v.get("msg_type").and_then(|x| x.as_str()) == Some("authorize") {
                        authorized = true;
                        info!("✅ Authorized successfully for {:?} mode", mode);
                        break;
                    }
                    if let Some(err) = v.get("error") {
                        return Err(anyhow!("Authorize error: {}", err));
                    }
                }
            }
            _ = sleep(Duration::from_millis(100)) => {
                if tokio::time::Instant::now() > deadline {
                    return Err(anyhow!("Authorize timeout"));
                }
            }
        }
    }
    
    if !authorized {
        return Err(anyhow!("Authorize timeout"));
    }
    
    Ok(WsClient {
        write,
        read,
        req_id: 0,
        current_mode: mode,
    })
}

/// Kirim request dan tunggu response dengan timeout yang lebih baik
async fn send_and_wait_msg_type<T: for<'de> Deserialize<'de>>(
    ws: &mut WsClient,
    payload: Value,
    expect_msg_type: &str,
    timeout_secs: u64,
) -> Result<T> {
    let mut to_send = payload.clone();
    let rid = ws.next_req_id() as i64;
    if let Some(obj) = to_send.as_object_mut() {
        obj.insert("req_id".into(), Value::from(rid));
    }
    
    ws.write.send(Message::Text(to_send.to_string())).await?;
    
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    
    loop {
        tokio::select! {
            maybe_msg = ws.read.next() => {
                match maybe_msg {
                    Some(Ok(msg)) => {
                        let txt = msg.to_text()?.to_string();
                        let v: Value = serde_json::from_str(&txt).unwrap_or(json!({}));
                        
                        if v.get("msg_type").and_then(|x| x.as_str()) == Some(expect_msg_type) {
                            let parsed: T = serde_json::from_value(v)?;
                            return Ok(parsed);
                        }
                        
                        // Log POC updates silently
                        if v.get("msg_type").and_then(|x| x.as_str()) == Some("proposal_open_contract") {
                            if let Ok(p) = serde_json::from_value::<PocResp>(v.clone()) {
                                if let Some(pi) = p.poc {
                                    if let (Some(cid), Some(st)) = (pi.contract_id, &pi.status) {
                                        if st != "open" {
                                            info!("  📊 Contract {} → {}", cid, st);
                                        }
                                    }
                                }
                            }
                        }
                        
                        // error?
                        if let Some(err) = v.get("error") {
                            return Err(anyhow!("Deriv API error: {}", err));
                        }
                    }
                    Some(Err(e)) => {
                        return Err(anyhow!("WebSocket error: {}", e));
                    }
                    None => {
                        return Err(anyhow!("WebSocket connection closed"));
                    }
                }
            }
            _ = sleep(Duration::from_millis(200)) => {
                if tokio::time::Instant::now() > deadline {
                    return Err(anyhow!("Timeout waiting for {}", expect_msg_type));
                }
            }
        }
    }
}

/// Buat proposal PUT (Fall).
async fn get_proposal_put(ws: &mut WsClient, amount: f64, currency: &str) -> Result<(String, f64)> {
    let payload = json!({
        "proposal": 1,
        "amount": amount,
        "basis": "stake",
        "contract_type": CONTRACT_TYPE_FALL,
        "currency": currency,
        "duration": DURATION_MINUTES,
        "duration_unit": "m",
        "symbol": SYMBOL
    });
    
    let resp: ProposalResp = send_and_wait_msg_type(ws, payload, "proposal", 20).await?;
    
    if let Some(err) = resp.error {
        return Err(anyhow!("Proposal error: {:?}", err));
    }
    
    let p = resp.proposal.ok_or_else(|| anyhow!("No proposal in response"))?;
    let id = p.id.ok_or_else(|| anyhow!("No proposal.id"))?;
    let price = p.ask_price.unwrap_or(amount);
    
    Ok((id, price))
}

/// Buat proposal PUT (Fall).
async fn get_proposal_call(ws: &mut WsClient, amount: f64, currency: &str) -> Result<(String, f64)> {
    let payload = json!({
        "proposal": 1,
        "amount": amount,
        "basis": "stake",
        "contract_type": CONTRACT_TYPE_RISE,
        "currency": currency,
        "duration": DURATION_MINUTES,
        "duration_unit": "m",
        "symbol": SYMBOL
    });
    
    let resp: ProposalResp = send_and_wait_msg_type(ws, payload, "proposal", 20).await?;
    
    if let Some(err) = resp.error {
        return Err(anyhow!("Proposal error: {:?}", err));
    }
    
    let p = resp.proposal.ok_or_else(|| anyhow!("No proposal in response"))?;
    let id = p.id.ok_or_else(|| anyhow!("No proposal.id"))?;
    let price = p.ask_price.unwrap_or(amount);
    
    Ok((id, price))
}

/// Helper: Bulatkan harga ke 2 desimal (requirement Deriv API)
fn round_to_2_decimals(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// Buy dari proposal_id, lalu subscribe POC hingga contract sold dan diketahui hasil.
async fn buy_and_wait_result(
    ws: &mut WsClient, 
    proposal_id: &str, 
    max_price: f64
) -> Result<(bool, f64, f64)> {
    // BUY - pastikan max_price sudah dibulatkan ke 2 desimal
    let max_price = round_to_2_decimals(max_price);
    let payload = json!({
        "buy": proposal_id,
        "price": max_price
    });
    
    let resp: BuyResponse = send_and_wait_msg_type(ws, payload, "buy", 20).await?;
    
    if let Some(err) = resp.error {
        return Err(anyhow!("Buy error: {:?}", err));
    }
    
    let buy = resp.buy.ok_or_else(|| anyhow!("No buy payload"))?;
    let contract_id = buy.contract_id.ok_or_else(|| anyhow!("No contract_id"))?;
    let buy_price = buy.buy_price.unwrap_or(0.0);
    
    info!("💰 Bought contract {} @ ${:.2}", contract_id, buy_price);
    
    // Subscribe POC
    let payload = json!({
        "proposal_open_contract": 1,
        "contract_id": contract_id,
        "subscribe": 1
    });
    ws.write.send(Message::Text(payload.to_string())).await?;
    
    // Tunggu hingga sold (max 90 detik untuk kontrak 1 menit)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    
    loop {
        tokio::select! {
            maybe = ws.read.next() => {
                match maybe {
                    Some(Ok(msg)) => {
                        let txt = msg.to_text()?.to_string();
                        let v: Value = serde_json::from_str(&txt).unwrap_or(json!({}));
                        
                        if v.get("msg_type").and_then(|x| x.as_str()) == Some("proposal_open_contract") {
                            let p: PocResp = serde_json::from_value(v)?;
                            if let Some(pi) = p.poc {
                                if pi.is_sold == Some(1) {
                                    let sell_price = pi.sell_price.unwrap_or(0.0);
                                    let status = pi.status.unwrap_or_else(|| "unknown".into());
                                    let profit = pi.profit.unwrap_or(sell_price - buy_price);
                                    
                                    let won = status == "won" || profit > 0.0;
                                    
                                    if won {
                                        info!("✅ Contract WON | Profit: ${:.2}", profit);
                                    } else {
                                        info!("❌ Contract LOST | Loss: ${:.2}", profit.abs());
                                    }
                                    
                                    return Ok((won, buy_price, sell_price));
                                }
                            }
                        } else if let Some(err) = v.get("error") {
                            return Err(anyhow!("POC error: {}", err));
                        }
                    }
                    Some(Err(e)) => {
                        return Err(anyhow!("WebSocket error: {}", e));
                    }
                    None => {
                        return Err(anyhow!("WebSocket closed during contract wait"));
                    }
                }
            },
            _ = sleep(Duration::from_millis(500)) => {
                if tokio::time::Instant::now() > deadline {
                    return Err(anyhow!("Timeout waiting for contract to settle"));
                }
            }
        }
    }
}

/// Lakukan satu transaksi Fall dan kembalikan bool won/lost.
async fn place_fall_trade(ws: &mut WsClient, stake: f64, currency: &str) -> Result<bool> {
    let (proposal_id, ask_price) = get_proposal_put(ws, stake, currency).await?;
    info!("📝 Proposal: ${:.2}", ask_price);
    
    // Tambahkan margin 10% dan bulatkan ke 2 desimal
    let max_price = round_to_2_decimals(ask_price * 1.10);
    
    let (won, _buy_price, _sell_price) = buy_and_wait_result(ws, &proposal_id, max_price).await?;
    
    Ok(won)
}

/// Lakukan satu transaksi Fall dan kembalikan bool won/lost.
async fn place_rise_trade(ws: &mut WsClient, stake: f64, currency: &str) -> Result<bool> {
    let (proposal_id, ask_price) = get_proposal_call(ws, stake, currency).await?;
    info!("📝 Proposal: ${:.2}", ask_price);
    
    // Tambahkan margin 10% dan bulatkan ke 2 desimal
    let max_price = round_to_2_decimals(ask_price * 1.10);
    
    let (won, _buy_price, _sell_price) = buy_and_wait_result(ws, &proposal_id, max_price).await?;
    
    Ok(won)
}

/// Jalankan mode DEMO: cari 4 kekalahan beruntun dengan persistent connection.
async fn run_demo_cycle(cfg: &AppCfg, ws: &mut WsClient) -> Result<()> {
    info!("");
    info!("🎮 ════════════════════════════════════════");
    info!("🎮 DEMO MODE: Mencari 4 kekalahan beruntun");
    info!("🎮 ════════════════════════════════════════");
    
    let mut losses_in_a_row = 0usize;
    let mut winner_in_a_row = 0usize;
    let mut trade_count = 0usize;
    
    loop {
        trade_count += 1;
        info!("");
        info!("🎯 DEMO Trade #{} | Streak: {}/4 losses - {}/4 winner", trade_count, losses_in_a_row, winner_in_a_row);
        info!("💵 FALL | 1m | ${}", STAKE_DEMO);
        
        let won = place_fall_trade(ws, STAKE_DEMO, &cfg.currency).await?;
        
        if !won {
            losses_in_a_row += 1;
            info!("📉 Loss recorded | Streak: {}/4", losses_in_a_row);
            winner_in_a_row = 0;
            
            if losses_in_a_row >= 4 {
                unsafe {
                    RISE = true;
                }

                info!("");
                info!("✨ ════════════════════════════════════════");
                info!("✨ TARGET REACHED: 4 losses in a row!");
                info!("✨ Switching to REAL MODE...");
                info!("✨ ════════════════════════════════════════");
                break;
            }
        } else {
            winner_in_a_row += 1;
            info!("📉 Win recorded | Streak: {}/4", winner_in_a_row);
            losses_in_a_row = 0;
            
            if winner_in_a_row >= 4 {
                unsafe {
                    RISE = false;
                }

                info!("");
                info!("✨ ════════════════════════════════════════");
                info!("✨ TARGET REACHED: 4 winner in a row!");
                info!("✨ Switching to REAL MODE...");
                info!("✨ ════════════════════════════════════════");
                break;
            }
        }
        
        // Jeda sebelum trade berikutnya
        sleep(Duration::from_secs(2)).await;
    }
    
    Ok(())
}

/// Jalankan mode REAL: Step1 (0.35). Kalau kalah → Step2 (0.79).
async fn run_real_cycle(cfg: &AppCfg, ws: &mut WsClient) -> Result<()> {
    info!("");
    info!("💎 ════════════════════════════════════════");
    info!("💎 REAL MODE");
    info!("💎 ════════════════════════════════════════");
    
    // Step 1
    info!("");
    info!("🎯 REAL Step");

    unsafe {
        if RISE {
            info!("💵 RISE | 1m | ${}", STAKE_REAL_STEP1);
            
            let won1 = place_rise_trade(ws, STAKE_REAL_STEP1, &cfg.currency).await?;
            
            if won1 {
                info!("");
                info!("🎉 ════════════════════════════════════════");
                info!("🎉 REAL Step WON! → Back to DEMO");
                info!("🎉 ════════════════════════════════════════");

                wait_for_minutes(5);

                return Ok(());
            } else {
                info!("⚠️  ════════════════════════════════════════");
                info!("⚠️  REAL Step 1 LOST → Back to DEMO");
                info!("⚠️  ════════════════════════════════════════");

            }
        } else {
            info!("💵 FALL | 1m | ${}", STAKE_REAL_STEP1);
            
            let won1 = place_fall_trade(ws, STAKE_REAL_STEP1, &cfg.currency).await?;
            
            if won1 {
                info!("");
                info!("🎉 ════════════════════════════════════════");
                info!("🎉 REAL Step WON! → Back to DEMO");
                info!("🎉 ════════════════════════════════════════");

                wait_for_minutes(5);

                return Ok(());
            } else {
                info!("⚠️  ════════════════════════════════════════");
                info!("⚠️  REAL Step 1 LOST → Back to DEMO");
                info!("⚠️  ════════════════════════════════════════");

            }

        }
    }
    
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive("sunderiv=info".parse().unwrap_or_default())
        )
        .init();
    
    let cfg = AppCfg {
        demo_app_id: env::var("DEMO_APP_ID").context("DEMO_APP_ID missing in .env")?,
        real_app_id: env::var("REAL_APP_ID").context("REAL_APP_ID missing in .env")?,
        demo_api_token: env::var("DEMO_DERIV_API").context("DEMO_DERIV_API missing in .env")?,
        real_api_token: env::var("REAL_DERIV_API").context("REAL_DERIV_API missing in .env")?,
        currency: env::var("CURRENCY").unwrap_or_else(|_| "USD".into()),
    };
    
    info!("");
    info!("🤖 ════════════════════════════════════════════════════════");
    info!("🤖 Deriv Trading Bot Started");
    info!("🤖 Currency: {} | Symbol: {}", cfg.currency, SYMBOL);
    info!("🤖 ════════════════════════════════════════════════════════");
    
    let mut cycle_count = 0usize;
    let mut current_mode = Mode::Demo;
    let mut ws_demo: Option<WsClient> = None;
    let mut ws_real: Option<WsClient> = None;

    // Siklus tanpa batas waktu dengan persistent connections
    loop {
        cycle_count += 1;
        info!("");
        info!("🔄 ═══════════════════════════════════════");
        info!("🔄 Starting Cycle #{}", cycle_count);
        info!("🔄 ═══════════════════════════════════════");
        
        // DEMO MODE
        loop {
            // Pastikan koneksi demo tersedia
            if ws_demo.is_none() {
                info!("🔌 Creating DEMO connection...");
                match connect_and_auth(&cfg.demo_app_id, &cfg.demo_api_token, Mode::Demo).await {
                    Ok(client) => {
                        ws_demo = Some(client);
                        current_mode = Mode::Demo;
                    }
                    Err(e) => {
                        error!("❌ Failed to connect DEMO: {}", e);
                        sleep(Duration::from_secs(5)).await;
                        continue;
                    }
                }
            }
            
            let ws = ws_demo.as_mut().unwrap();
            
            match run_demo_cycle(&cfg, ws).await {
                Ok(_) => {
                    // Demo berhasil, lanjut ke real
                    break;
                }
                Err(e) => {
                    error!("❌ Demo cycle error: {}", e);
                    warn!("🔄 Reconnecting DEMO in 3 seconds...");
                    ws_demo = None; // Force reconnect
                    sleep(Duration::from_secs(3)).await;
                }
            }
        }
        
        sleep(Duration::from_secs(2)).await;
        
        // REAL MODE
        loop {
            // Pastikan koneksi real tersedia
            if ws_real.is_none() {
                info!("🔌 Creating REAL connection...");
                match connect_and_auth(&cfg.real_app_id, &cfg.real_api_token, Mode::Real).await {
                    Ok(client) => {
                        ws_real = Some(client);
                        current_mode = Mode::Real;
                    }
                    Err(e) => {
                        error!("❌ Failed to connect REAL: {}", e);
                        sleep(Duration::from_secs(5)).await;
                        continue;
                    }
                }
            }
            
            let ws = ws_real.as_mut().unwrap();
            
            match run_real_cycle(&cfg, ws).await {
                Ok(_) => {
                    // Real selesai, kembali ke demo
                    current_mode = Mode::Demo;
                    break;
                }
                Err(e) => {
                    error!("❌ Real cycle error: {}", e);
                    warn!("🔄 Reconnecting REAL in 3 seconds...");
                    ws_real = None; // Force reconnect
                    sleep(Duration::from_secs(3)).await;
                }
            }
        }
        
        // Jeda sebelum cycle berikutnya
        info!("⏳ Waiting 2 seconds before next cycle...");
        sleep(Duration::from_secs(2)).await;
    }
}