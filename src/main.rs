use std::{collections::HashMap, fs, sync::Arc, env, time::Duration};
use teloxide::{prelude::*, types::Message};
use teloxide::utils::command::BotCommands;
use serde::Deserialize;
use wake_on_lan::MagicPacket;
use mac_address::MacAddress;
use tokio::process::Command as AsyncCommand;

// --- СТРУКТУРЫ ---
#[derive(Deserialize)]
struct FileConfig {
    allowed_users: Vec<u64>,
    devices: HashMap<String, Vec<String>>,
}

struct DeviceInfo {
    mac: [u8; 6],
    ip: String,
}

struct SafeConfig {
    allowed_users: Vec<UserId>,
    devices: HashMap<String, DeviceInfo>,
}

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase")]
enum Command {
    #[command(description = "show help")] Help,
    #[command(description = "list devices")] List,
    #[command(description = "wake device")] Wake(String),
    #[command(description = "check status")] Status(String),
}

// --- ФУНКЦИИ ---
async fn check_online(ip: &str, name: &str) -> bool {
    println!("[PING] Checking status for {} ({})", name, ip);
    let ok = AsyncCommand::new("ping")
        .args(["-c", "1", "-W", "1", ip])
        .output()
        .await
        .map(|res| res.status.success())
        .unwrap_or(false);
    
    println!("[PING] Result for {}: {}", name, if ok { "ONLINE" } else { "OFFLINE" });
    ok
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    println!("--- Starting WOL Bot ---");

    let bot_token = env::var("TELOXIDE_TOKEN")
        .map(|t| t.trim().to_string())
        .expect("TELOXIDE_TOKEN not set");
    
    let config_raw = fs::read_to_string("config.toml").expect("Missing config.toml");
    let file_config: FileConfig = toml::from_str(&config_raw).expect("Invalid config.toml");

    let mut devices = HashMap::new();
    for (name, params) in file_config.devices {
        if params.len() == 2 {
            if let Ok(mac) = params[0].parse::<MacAddress>() {
                devices.insert(name.clone(), DeviceInfo { mac: mac.bytes(), ip: params[1].clone() });
                println!("[INIT] Device loaded: {} ({})", name, params[1]);
            }
        }
    }

    let config = Arc::new(SafeConfig {
        allowed_users: file_config.allowed_users.into_iter().map(UserId).collect(),
        devices,
    });

    println!("[INIT] Allowed users: {}", config.allowed_users.len());
    println!("[INIT] Bot is ready and listening...");

    let bot = Bot::new(bot_token);

    Command::repl(bot, move |bot: Bot, msg: Message, cmd: Command| {
        let conf = config.clone();
        async move {
            let user = msg.from();
            let user_id = user.map(|u| u.id).unwrap_or(UserId(0));
            let username = user.and_then(|u| u.username.as_deref()).unwrap_or("unknown");

            // SECURITY LOG
            if !conf.allowed_users.contains(&user_id) {
                eprintln!("[UNAUTHORIZED] ID: {} (@{}) tried to use the bot", user_id, username);
                return Ok(());
            }

            match cmd {
                Command::Help => {
                    println!("[CMD] Help requested by {}", user_id);
                    bot.send_message(msg.chat.id, Command::descriptions().to_string()).await?;
                }
                Command::List => {
                    println!("[CMD] List requested by {}", user_id);
                    let mut res = String::from("🖥 <b>Devices:</b>\n");
                    for name in conf.devices.keys() {
                        res.push_str(&format!("• <code>{}</code>\n", name));
                    }
                    bot.send_message(msg.chat.id, res).parse_mode(teloxide::types::ParseMode::Html).await?;
                }
                Command::Status(name) => {
                    println!("[CMD] Status check for '{}' by {}", name, user_id);
                    if let Some(info) = conf.devices.get(&name) {
                        let is_up = check_online(&info.ip, &name).await;
                        bot.send_message(msg.chat.id, if is_up { "✅ Online" } else { "💤 Offline" }).await?;
                    } else {
                        bot.send_message(msg.chat.id, "❌ Device not found").await?;
                    }
                }
                Command::Wake(name) => {
                    println!("[CMD] Wake request for '{}' by {}", name, user_id);
                    if let Some(info) = conf.devices.get(&name) {
                        let _ = MagicPacket::new(&info.mac).send();
                        println!("[WOL] Magic Packet sent to {}", name);
                        
                        bot.send_message(msg.chat.id, format!("🚀 Waking {}...", name)).await?;
                        
                        let b = bot.clone();
                        let ip = info.ip.clone();
                        let cid = msg.chat.id;
                        let n = name.clone();

                        tokio::spawn(async move {
                            println!("[TASK] Waiting 30s to verify {}...", n);
                            tokio::time::sleep(Duration::from_secs(30)).await;
                            let is_up = check_online(&ip, &n).await;
                            let _ = b.send_message(cid, if is_up { 
                                format!("✅ <b>{}</b> is now UP!", n) 
                            } else { 
                                format!("⚠️ <b>{}</b> no response after 30s", n) 
                            }).parse_mode(teloxide::types::ParseMode::Html).await;
                        });
                    } else {
                        bot.send_message(msg.chat.id, "❌ Device not found").await?;
                    }
                }
            };
            Ok(())
        }
    }).await;
}
