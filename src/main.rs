use serde::Deserialize;
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::net::UdpSocket;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use teloxide::prelude::*;
use teloxide::types::ParseMode;

#[derive(Deserialize, Debug, Clone)]
struct Config {
    allowed_users: Vec<u64>,
    interface: Option<String>,
    devices: HashMap<String, (String, String)>,
}

fn create_magic_packet(mac: &str) -> Result<Vec<u8>, String> {
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

fn create_wol_socket(interface: Option<&str>) -> std::io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_broadcast(true)?;

    if let Some(iface) = interface {
        #[cfg(target_os = "linux")]
        {
            if let Err(e) = socket.bind_device(Some(iface.as_bytes())) {
                eprintln!("[ERROR] Failed to bind to interface {}: {}", iface, e);
            } else {
                println!("[INFO] Socket bound to interface: {}", iface);
            }
        }
    }
    Ok(socket.into())
}

async fn is_device_online(ip: &str) -> bool {
    let status = Command::new("ping")
        .args(["-c", "1", "-W", "1", ip])
        .status();
    match status {
        Ok(s) => s.success(),
        Err(_) => false,
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = env::args().collect();
    let config_path = args.get(1).map(|s| s.as_str()).unwrap_or("config.toml");

    println!("[START] Launching WOL Bot. Config: {}", config_path);

    let content = match fs::read_to_string(config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[FATAL] Could not read config file {}: {}", config_path, e);
            return;
        }
    };

    let config: Arc<Config> = Arc::new(match toml::from_str(&content) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[FATAL] TOML parse error: {}", e);
            return;
        }
    });

    let bot = Bot::from_env();

    let handler = Update::filter_message().endpoint(
        move |bot: Bot, config: Arc<Config>, msg: Message| async move {
            let user_id = msg.from().map(|u| u.id.0).unwrap_or(0);
            
            if !config.allowed_users.contains(&user_id) {
                println!("[AUTH] Access denied for user ID: {}", user_id);
                return Ok(());
            }

            let text = msg.text().unwrap_or_default();
            let parts: Vec<&str> = text.split_whitespace().collect();
            let cmd = parts.get(0).copied().unwrap_or("");

            match cmd {
                "/start" | "/help" => {
                    let help = "🤖 *WOL Bot Menu*\n\n\
                                `/list` — List configured devices\n\
                                `/status_all` — Ping all devices\n\
                                `/status <name>` — Ping specific device\n\
                                `/wake <name>` — Send Magic Packet";
                    bot.send_message(msg.chat.id, help).parse_mode(ParseMode::Markdown).await?;
                }

                "/list" => {
                    let mut list = String::from("📋 *Configured Devices:*\n");
                    for name in config.devices.keys() {
                        list.push_str(&format!("• `{}`\n", name));
                    }
                    bot.send_message(msg.chat.id, list).parse_mode(ParseMode::Markdown).await?;
                }

                "/status_all" => {
                    println!("[CMD] Bulk status check requested by {}", user_id);
                    let mut report = String::from("🔍 *Network Status:*\n");
                    for (name, (_, ip)) in &config.devices {
                        let status = if is_device_online(ip).await { "✅ ONLINE" } else { "🔴 OFFLINE" };
                        report.push_str(&format!("• `{}`: {}\n", name, status));
                    }
                    bot.send_message(msg.chat.id, report).parse_mode(ParseMode::Markdown).await?;
                }

                "/status" => {
                    if let Some(name) = parts.get(1) {
                        if let Some((_, ip)) = config.devices.get(*name) {
                            let status = if is_device_online(ip).await { "✅ ONLINE" } else { "🔴 OFFLINE" };
                            bot.send_message(msg.chat.id, format!("Device `{}` is {}", name, status)).parse_mode(ParseMode::Markdown).await?;
                        }
                    }
                }

                "/wake" => {
                    if let Some(name) = parts.get(1) {
                        if let Some((mac, ip)) = config.devices.get(*name) {
                            println!("[WAKE] Waking up {} ({})", name, mac);
                            
                            let packet = create_magic_packet(mac).unwrap();
                            let socket = create_wol_socket(config.interface.as_deref()).unwrap();
                            
                            if let Err(e) = socket.send_to(&packet, "255.255.255.255:9") {
                                eprintln!("[ERROR] Failed to send packet: {}", e);
                                bot.send_message(msg.chat.id, "❌ Error: Failed to send Magic Packet").await?;
                                return Ok(());
                            }
                            
                            bot.send_message(msg.chat.id, format!("🚀 Magic Packet sent to `{}`. Verifying in 30s...", name)).parse_mode(ParseMode::Markdown).await?;

                            tokio::time::sleep(Duration::from_secs(30)).await;
                            
                            if is_device_online(ip).await {
                                bot.send_message(msg.chat.id, format!("✅ `{}` is now ONLINE!", name)).parse_mode(ParseMode::Markdown).await?;
                            } else {
                                bot.send_message(msg.chat.id, format!("⚠️ `{}` is still not responding to ping.", name)).parse_mode(ParseMode::Markdown).await?;
                            }
                        } else {
                            bot.send_message(msg.chat.id, "❌ Device not found in config.").await?;
                        }
                    }
                }
                _ => {}
            }
            Ok(())
        },
    );

    Dispatcher::builder(bot, handler).build().dispatch().await;
}
