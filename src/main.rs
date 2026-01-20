use serde::Deserialize;
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::net::UdpSocket;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use teloxide::prelude::*;
use teloxide::types::ParseMode;

/// Configuration structure mapped from the TOML file
#[derive(Deserialize, Debug, Clone)]
struct Config {
    allowed_users: Vec<u64>,               // Telegram User IDs permitted to use the bot
    interface: Option<String>,             // Network interface (e.g., "br-lan")
    devices: HashMap<String, (String, String, String)>, // Device name -> (MAC Address, IP Address, Timeout)
}

/// Helper function to generate a Unix timestamp string for logging
fn get_time() -> String {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{}", now)
}

/// Macro for standardized info logging to stdout
macro_rules! log_info {
    ($($arg:tt)*) => {
        println!("[{}] [INFO] {}", get_time(), format!($($arg)*));
    };
}

/// Macro for standardized error logging to stderr
macro_rules! log_err {
    ($($arg:tt)*) => {
        eprintln!("[{}] [ERROR] {}", get_time(), format!($($arg)*));
    };
}

/// Constructs a Wake-on-LAN Magic Packet
/// A Magic Packet consists of 6 bytes of 0xFF followed by 16 repetitions of the target MAC
fn create_magic_packet(mac: &str) -> Result<Vec<u8>, String> {
    // Parse MAC string (e.g., "AA:BB:CC...") into bytes
    let mac_bytes: Vec<u8> = mac
        .split(|c| c == ':' || c == '-')
        .filter(|s| !s.is_empty())
        .map(|b| u8::from_str_radix(b, 16))
        .collect::<Result<Vec<u8>, _>>()
        .map_err(|_| "Invalid MAC address format".to_string())?;

    if mac_bytes.len() != 6 {
        return Err("MAC address must be exactly 6 bytes".to_string());
    }

    let mut packet = vec![0xFF; 6];
    for _ in 0..16 {
        packet.extend_from_slice(&mac_bytes);
    }
    Ok(packet)
}

/// Creates a UDP socket and binds it to a specific physical interface
/// Binding to an interface (like br-lan) ensures the packet stays within the local network
fn create_wol_socket(interface: Option<&str>) -> std::io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_broadcast(true)?; // Required to send to 255.255.255.255

    if let Some(iface) = interface {
        #[cfg(target_os = "linux")]
        {
            // Binds the socket to a device (MIPS/OpenWrt specific optimization)
            if let Err(e) = socket.bind_device(Some(iface.as_bytes())) {
                log_err!("Failed to bind to interface {}: {}", iface, e);
            } else {
                log_info!("Socket successfully bound to interface: {}", iface);
            }
        }
    }
    Ok(socket.into())
}

/// Executes a system 'ping' command to check if a device is reachable
async fn is_device_online(ip: &str) -> bool {
    log_info!("Pinging IP: {}...", ip);
    // -c 1: one packet, -W 1: one second timeout
    let status = Command::new("ping")
        .args(["-c", "1", "-W", "1", ip])
        .status();
    match status {
        Ok(s) => s.success(),
        Err(e) => {
            log_err!("Ping command failed for {}: {}", ip, e);
            false
        }
    }
}

#[tokio::main(flavor = "current_thread")] // Single-threaded runtime to save RAM on MT7621
async fn main() {
    // 1. Collect CLI arguments to find the config file path
    let args: Vec<String> = env::args().collect();
    let config_path = args.get(1).map(|s| s.as_str()).unwrap_or("config.toml");

    log_info!("Starting WOL Bot. Target config: {}", config_path);

    // 2. Read and parse the TOML configuration
    let content = match fs::read_to_string(config_path) {
        Ok(c) => c,
        Err(e) => {
            log_err!("FATAL: Could not read config file {}: {}", config_path, e);
            return;
        }
    };

    // Use the turbofish operator ::<Config> to clarify the target type
    let config: Arc<Config> = Arc::new(match toml::from_str::<Config>(&content) {
        Ok(c) => {
            log_info!("Configuration loaded. Monitoring {} devices.", c.devices.len());
            c
        },
        Err(e) => {
            log_err!("FATAL: TOML parse error: {}", e);
            return;
        }
    });

    // 3. Initialize Telegram Bot client (Token is pulled from TELOXIDE_TOKEN env var)
    let bot = Bot::from_env();
    log_info!("Telegram Bot client initialized.");

    // 4. Define the message processing logic
    let handler = Update::filter_message().endpoint(
        move |bot: Bot, config: Arc<Config>, msg: Message| async move {
            let user = msg.from();
            let user_id = user.map(|u| u.id.0).unwrap_or(0);
            let username = user.and_then(|u| u.username.as_deref()).unwrap_or("unknown");

            // Log every incoming command for audit
            if let Some(text) = msg.text() {
                log_info!("Message from {} (ID: {}): {}", username, user_id, text);
            }

            // Security check: drop requests from unauthorized users
            if !config.allowed_users.contains(&user_id) {
                log_err!("AUTH DENIED: User {} (ID: {}) is not authorized.", username, user_id);
                return ResponseResult::Ok(());
            }

            let text = msg.text().unwrap_or_default();
            let parts: Vec<&str> = text.split_whitespace().collect();
            let cmd = parts.get(0).copied().unwrap_or("");

            match cmd {
                "/start" | "/help" => {
                    bot.send_message(msg.chat.id, "<b>🤖 WOL Bot Menu</b>\n\n<code>/list</code>, <code>/status_all</code>, <code>/wake &lt;name&gt;</code>")
                        .parse_mode(ParseMode::Html).await?;
                }

                "/list" => {
                    log_info!("User {} requested device list.", username);
                    let mut list = String::from("<b>📋 Configured Devices:</b>\n");
                    for name in config.devices.keys() {
                        list.push_str(&format!("• <code>{}</code>\n", name));
                    }
                    bot.send_message(msg.chat.id, list).parse_mode(ParseMode::Html).await?;
                }

                "/status_all" => {
                    log_info!("User {} requested bulk status check.", username);
                    let mut report = String::from("<b>🔍 Network Status:</b>\n");
                    for (name, (_, ip, _)) in &config.devices {
                        let online = is_device_online(ip).await;
                        let status = if online { "✅ ONLINE" } else { "🔴 OFFLINE" };
                        log_info!("Device {}({}) status: {}", name, ip, status);
                        report.push_str(&format!("• <code>{}</code>: {}\n", name, status));
                    }
                    bot.send_message(msg.chat.id, report).parse_mode(ParseMode::Html).await?;
                }

                "/status" => {
                    if let Some(name) = parts.get(1) {
                        if let Some((_, ip, _)) = config.devices.get(*name) {
                            let online = is_device_online(ip).await;
                            let status = if online { "✅ ONLINE" } else { "🔴 OFFLINE" };
                            log_info!("Single status check for {}: {}", name, status);
                            bot.send_message(msg.chat.id, format!("Device <code>{}</code> is {}", name, status))
                                .parse_mode(ParseMode::Html).await?;
                        }
                    }
                }

                "/wake" => {
                    if let Some(name) = parts.get(1) {
                        if let Some((mac, ip, timeout_str)) = config.devices.get(*name) {
                            let timeout_secs: u64 = timeout_str.parse().unwrap_or(30);
                            log_info!("WAKE REQUEST: User {} is waking {} ({}), timeout: {}", username, name, mac, timeout_secs);
                            
                            // Prepare packet and socket
                            let packet = create_magic_packet(mac).unwrap();
                            let socket = create_wol_socket(config.interface.as_deref()).unwrap();
                            
                            // Send to broadcast address on port 9 (standard WOL port)
                            match socket.send_to(&packet, "255.255.255.255:9") {
                                Ok(_) => {
                                    log_info!("Magic Packet successfully broadcasted for {}.", name);
                                    bot.send_message(msg.chat.id, format!("🚀 Packet sent to <code>{}</code>. Verifying in {}s...", name, timeout_secs))
                                        .parse_mode(ParseMode::Html).await?;
                                },
                                Err(e) => {
                                    log_err!("Failed to send Magic Packet for {}: {}", name, e);
                                    bot.send_message(msg.chat.id, "❌ Network error.").await?;
                                    return ResponseResult::Ok(());
                                }
                            }

                            // Wait for the OS to boot up before checking status
                            tokio::time::sleep(Duration::from_secs(timeout_secs)).await;
                            
                            let final_status = if is_device_online(ip).await { "✅ ONLINE" } else { "⚠️ STILL OFFLINE" };
                            log_info!("Post-wake verification for {}: {}", name, final_status);
                            bot.send_message(msg.chat.id, format!("Result for <code>{}</code>: {}", name, final_status))
                                .parse_mode(ParseMode::Html).await?;
                        } else {
                            log_err!("WAKE FAILED: Device '{}' not found.", name);
                            bot.send_message(msg.chat.id, "❌ Device not found.").await?;
                        }
                    }
                }
                _ => {}
            }
            ResponseResult::Ok(())
        },
    );

    // 5. Start the event dispatcher
    // Dependencies are injected here so they can be accessed inside the handler above
    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![config])
        .enable_ctrlc_handler() // Allows clean shutdown with Ctrl+C
        .build()
        .dispatch()
        .await;
}
