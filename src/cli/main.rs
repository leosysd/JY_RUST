#[path = "../config.rs"]
mod config;
#[path = "../clob.rs"]
mod clob;
#[path = "../state.rs"]
mod state;
#[path = "../signing.rs"]
mod signing;
#[path = "../ws.rs"]
mod ws;
#[path = "../strategy/mod.rs"]
mod strategy;

use anyhow::Result;
use console::style;
use dialoguer::{theme::ColorfulTheme, Confirm, Input, Password, Select};
use std::path::PathBuf;

const ENV_PATH: &str = "/opt/polymarket-copy/.env";
const SERVICE: &str = "jy-bot";

fn theme() -> ColorfulTheme {
    ColorfulTheme::default()
}

fn main() -> Result<()> {
    // 如果有子命令直接处理
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 {
        return handle_subcommand(&args[1..]);
    }
    interactive_menu()
}

fn handle_subcommand(args: &[String]) -> Result<()> {
    match args[0].as_str() {
        "service" => {
            let action = args.get(1).map(|s| s.as_str()).unwrap_or("status");
            service_cmd(action);
        }
        "logs" => show_logs(50),
        "status" => service_cmd("status"),
        "start" => service_cmd("start"),
        "stop" => service_cmd("stop"),
        "restart" => service_cmd("restart"),
        "set-dry-run" => {
            let val = args.get(1).map(|s| s.as_str()).unwrap_or("1");
            set_env_val("DRY_RUN", val);
            println!("{} DRY_RUN={val}", style("✔").green());
            if args.contains(&"--restart".to_string()) {
                service_cmd("restart");
            }
        }
        "set-bot-mode" => {
            let val = args.get(1).map(|s| s.as_str()).unwrap_or("quant");
            set_env_val("QUANT_STRATEGY", val);
            println!("{} QUANT_STRATEGY={val}", style("✔").green());
        }
        "update" => update_bot()?,
        _ => {
            eprintln!("未知命令: {}", args[0]);
            std::process::exit(1);
        }
    }
    Ok(())
}

fn interactive_menu() -> Result<()> {
    loop {
        println!();
        println!("{}", style("══════════════════════════════").cyan());
        println!("{}  {}", style("🤖").bold(), style("JY Bot 管理菜单").bold().cyan());
        println!("{}", style("══════════════════════════════").cyan());

        // 显示当前状态
        let running = is_service_running();
        let dry_run = read_env_val("DRY_RUN").unwrap_or("1".into());
        let mode = read_env_val("QUANT_STRATEGY").unwrap_or("jetfadil".into());
        println!(
            "  服务: {}  模式: {}  DRY_RUN: {}",
            if running { style("运行中").green().bold() } else { style("已停止").red().bold() },
            style(&mode).yellow(),
            if dry_run == "0" { style("实盘").red().bold() } else { style("模拟").green() },
        );
        println!();

        let items = vec![
            "1. 初始化/修改配置",
            "2. 查看当前配置",
            "3. 测试 API 连接",
            "4. 启动服务",
            "5. 停止服务",
            "6. 重启服务",
            "7. 查看实时日志",
            "8. 切换 DRY_RUN 模式",
            "9. 切换策略（jetfadil / copy）",
            "u. 更新程序（从 GitHub 拉取最新版本）",
            "0. 退出",
        ];

        let sel = Select::with_theme(&theme())
            .with_prompt("选择操作")
            .items(&items)
            .default(0)
            .interact()?;

        println!();
        match sel {
            0 => edit_config()?,
            1 => show_config(),
            2 => test_connection()?,
            3 => service_cmd("start"),
            4 => service_cmd("stop"),
            5 => service_cmd("restart"),
            6 => {
                show_logs(30);
                println!("\n{}", style("（按 Ctrl+C 停止日志）").dim());
                live_logs();
            }
            7 => toggle_dry_run()?,
            8 => toggle_strategy()?,
            9 => update_bot()?,
            10 => break,
            _ => {}
        }
    }
    Ok(())
}

fn edit_config() -> Result<()> {
    println!("{}", style("── 初始化/修改配置 ──").bold());

    let curr = |key: &str| read_env_val(key).unwrap_or_default();

    let bot_mode: String = Input::with_theme(&theme())
        .with_prompt("BOT_MODE (jetfadil/copy)")
        .default(curr("QUANT_STRATEGY").into())
        .interact_text()?;

    let target_wallet: String = Input::with_theme(&theme())
        .with_prompt("TARGET_WALLET (跟单目标地址)")
        .default(curr("TARGET_WALLET").into())
        .allow_empty(true)
        .interact_text()?;

    let private_key: String = Password::with_theme(&theme())
        .with_prompt("PRIVATE_KEY (留空保持不变)")
        .allow_empty_password(true)
        .interact()?;

    let deposit_wallet: String = Input::with_theme(&theme())
        .with_prompt("DEPOSIT_WALLET_ADDRESS")
        .default(curr("DEPOSIT_WALLET_ADDRESS").into())
        .allow_empty(true)
        .interact_text()?;

    let dry_run_choice = Select::with_theme(&theme())
        .with_prompt("DRY_RUN")
        .items(&["1 - 模拟（推荐先用这个）", "0 - 实盘下单"])
        .default(if curr("DRY_RUN") == "0" { 1 } else { 0 })
        .interact()?;

    let copy_ratio: String = Input::with_theme(&theme())
        .with_prompt("COPY_RATIO（跟单比例，仅 copy 模式）")
        .default(curr("COPY_RATIO").if_empty("1.0").into())
        .interact_text()?;

    // 写入 .env
    set_env_val("QUANT_STRATEGY", &bot_mode);
    if !target_wallet.is_empty() { set_env_val("TARGET_WALLET", &target_wallet); }
    if !private_key.is_empty() { set_env_val("PRIVATE_KEY", &private_key); }
    if !deposit_wallet.is_empty() { set_env_val("DEPOSIT_WALLET_ADDRESS", &deposit_wallet); }
    set_env_val("DRY_RUN", if dry_run_choice == 0 { "1" } else { "0" });
    set_env_val("COPY_RATIO", &copy_ratio);

    println!("{} 配置已保存到 {ENV_PATH}", style("✔").green());
    Ok(())
}

fn show_config() {
    println!("{}", style("── 当前配置 ──").bold());
    let keys = [
        "QUANT_STRATEGY", "DRY_RUN", "TARGET_WALLET",
        "DEPOSIT_WALLET_ADDRESS", "COPY_RATIO",
        "QUANT_ORDER_SHARES", "QUANT_ARBITRAGE_MIN_PROFIT",
        "JF_MAX_ENTRY_DELAY_SEC", "CLOB_API_URL",
    ];
    for key in &keys {
        let val = read_env_val(key).unwrap_or_else(|| "(未设置)".into());
        let display = if key.contains("KEY") || key.contains("PRIVATE") {
            if val.len() > 10 { format!("{}...{}", &val[..6], &val[val.len()-4..]) } else { "(已设置)".into() }
        } else { val };
        println!("  {:<35} = {}", style(key).dim(), style(display).white());
    }
}

fn test_connection() -> Result<()> {
    println!("{}", style("── 测试连接 ──").bold());
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let cfg = config::load(Some(ENV_PATH)).unwrap();
        let client = clob::ClobClient::new(&cfg.clob_api_url, &cfg.gamma_api_url, &cfg.market_slug_prefix);
        print!("  CLOB API... ");
        match client.find_current_market().await {
            Some(m) => println!("{} 找到市场: {}", style("✔").green(), m.title),
            None => println!("{} 未找到当前市场（可能是非交易时间）", style("⚠").yellow()),
        }
    });
    Ok(())
}

fn toggle_dry_run() -> Result<()> {
    let curr = read_env_val("DRY_RUN").unwrap_or("1".into());
    let is_dry = curr != "0";
    let choice = Select::with_theme(&theme())
        .with_prompt("选择模式")
        .items(&["1 - 模拟（不真实下单）", "0 - 实盘下单⚠️"])
        .default(if is_dry { 0 } else { 1 })
        .interact()?;
    let new_val = if choice == 0 { "1" } else { "0" };
    if new_val == "0" {
        let confirm = Confirm::with_theme(&theme())
            .with_prompt("确认切换到实盘？这会使用真实资金下单！")
            .default(false)
            .interact()?;
        if !confirm { return Ok(()); }
    }
    set_env_val("DRY_RUN", new_val);
    println!("{} DRY_RUN={new_val}", style("✔").green());
    if Confirm::with_theme(&theme()).with_prompt("重启服务？").default(true).interact()? {
        service_cmd("restart");
    }
    Ok(())
}

fn toggle_strategy() -> Result<()> {
    let curr = read_env_val("QUANT_STRATEGY").unwrap_or("jetfadil".into());
    let choice = Select::with_theme(&theme())
        .with_prompt("选择策略")
        .items(&[
            "jetfadil - 锁利策略（复刻 JetFadil，推荐）",
            "copy     - 跟单模式（直接复制目标地址交易）",
            "arb      - 纯套利（仅在两边合价 < 0.935 时入场）",
        ])
        .default(match curr.as_str() { "copy" => 1, "arb" => 2, _ => 0 })
        .interact()?;
    let new_val = match choice { 1 => "copy", 2 => "arb", _ => "jetfadil" };
    set_env_val("QUANT_STRATEGY", new_val);
    println!("{} QUANT_STRATEGY={new_val}", style("✔").green());
    if Confirm::with_theme(&theme()).with_prompt("重启服务？").default(true).interact()? {
        service_cmd("restart");
    }
    Ok(())
}

// ── 工具函数 ────────────────────────────────────────────────────────────────

fn service_cmd(action: &str) {
    let status = std::process::Command::new("sudo")
        .args(["systemctl", action, SERVICE])
        .status();
    match status {
        Ok(s) if s.success() => println!("{} systemctl {action} {SERVICE}", style("✔").green()),
        Ok(s) => println!("{} 退出码: {}", style("✖").red(), s.code().unwrap_or(-1)),
        Err(e) => println!("{} {e}", style("✖").red()),
    }
    // 短暂等待后显示状态
    std::thread::sleep(std::time::Duration::from_millis(800));
    let _ = std::process::Command::new("sudo")
        .args(["systemctl", "status", SERVICE, "--no-pager", "-l"])
        .spawn()
        .and_then(|mut c| c.wait());
}

fn show_logs(lines: usize) {
    let _ = std::process::Command::new("sudo")
        .args(["journalctl", "-u", SERVICE, "--no-pager", "-n", &lines.to_string()])
        .status();
}

fn live_logs() {
    let _ = std::process::Command::new("sudo")
        .args(["journalctl", "-u", SERVICE, "-f", "--no-pager"])
        .status();
}

fn is_service_running() -> bool {
    std::process::Command::new("sudo")
        .args(["systemctl", "is-active", "--quiet", SERVICE])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn read_env_val(key: &str) -> Option<String> {
    if std::path::Path::new(ENV_PATH).exists() {
        if let Ok(content) = std::fs::read_to_string(ENV_PATH) {
            for line in content.lines() {
                if let Some(rest) = line.strip_prefix(&format!("{key}=")) {
                    return Some(rest.trim().to_string());
                }
            }
        }
    }
    None
}

fn set_env_val(key: &str, val: &str) {
    let path = PathBuf::from(ENV_PATH);
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
    let entry = format!("{key}={val}");
    let mut found = false;
    for line in &mut lines {
        if line.starts_with(&format!("{key}=")) || line.starts_with(&format!("#{key}=")) {
            *line = entry.clone();
            found = true;
            break;
        }
    }
    if !found {
        lines.push(entry);
    }
    let _ = std::fs::write(&path, lines.join("\n") + "\n");
}

fn update_bot() -> Result<()> {
    const REPO: &str = "https://github.com/leosysd/JY_RUST.git";
    const INSTALL_DIR: &str = "/opt/jy-rust";

    println!("{}", style("── 更新程序 ──").bold());
    println!("  来源: {}", style(REPO).dim());
    println!();

    // 确认
    let ok = Confirm::with_theme(&theme())
        .with_prompt("从 GitHub 拉取最新版本并重新编译？（服务将短暂停止）")
        .default(true)
        .interact()?;
    if !ok {
        return Ok(());
    }

    println!("{} 停止服务...", style("[1/4]").cyan());
    let _ = std::process::Command::new("sudo")
        .args(["systemctl", "stop", SERVICE])
        .status();

    println!("{} 拉取最新代码...", style("[2/4]").cyan());
    let pull_status = if std::path::Path::new(&format!("{}/.git", INSTALL_DIR)).exists() {
        std::process::Command::new("git")
            .args(["-C", INSTALL_DIR, "pull", "--ff-only"])
            .status()
    } else {
        std::process::Command::new("git")
            .args(["clone", REPO, INSTALL_DIR])
            .status()
    };

    match pull_status {
        Ok(s) if s.success() => println!("  {} 代码更新成功", style("✔").green()),
        Ok(s) => {
            println!("  {} git 退出码: {}", style("✖").red(), s.code().unwrap_or(-1));
            println!("  正在重启服务（使用旧版本）...");
            let _ = std::process::Command::new("sudo")
                .args(["systemctl", "start", SERVICE])
                .status();
            return Ok(());
        }
        Err(e) => {
            println!("  {} {e}", style("✖").red());
            let _ = std::process::Command::new("sudo")
                .args(["systemctl", "start", SERVICE])
                .status();
            return Ok(());
        }
    }

    println!("{} 编译中（需要 1-2 分钟）...", style("[3/4]").cyan());

    // 确保 cargo 在 PATH 中
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    let cargo_path = format!("{home}/.cargo/bin");
    let current_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{cargo_path}:{current_path}");

    let build_status = std::process::Command::new("cargo")
        .args(["build", "--release"])
        .current_dir(INSTALL_DIR)
        .env("PATH", &new_path)
        .status();

    match build_status {
        Ok(s) if s.success() => println!("  {} 编译成功", style("✔").green()),
        _ => {
            println!("  {} 编译失败，使用旧版本重启", style("✖").red());
            let _ = std::process::Command::new("sudo")
                .args(["systemctl", "start", SERVICE])
                .status();
            return Ok(());
        }
    }

    println!("{} 安装并重启服务...", style("[4/4]").cyan());
    let bin_dir = format!("{INSTALL_DIR}/target/release");
    let _ = std::process::Command::new("sudo")
        .args(["cp", &format!("{bin_dir}/jy-bot"), "/usr/local/bin/jy-bot"])
        .status();
    let _ = std::process::Command::new("sudo")
        .args(["cp", &format!("{bin_dir}/jy"), "/usr/local/bin/jy"])
        .status();

    let _ = std::process::Command::new("sudo")
        .args(["systemctl", "start", SERVICE])
        .status();

    println!();
    println!("{} 更新完成！", style("✔").green().bold());

    // 显示新版本的 git log
    if let Ok(out) = std::process::Command::new("git")
        .args(["-C", INSTALL_DIR, "log", "--oneline", "-3"])
        .output()
    {
        println!("  最新提交:\n  {}", String::from_utf8_lossy(&out.stdout).trim().replace('\n', "\n  "));
    }

    Ok(())
}

trait StrExt {
    fn if_empty<'a>(&'a self, default: &'a str) -> &'a str;
}
impl StrExt for str {
    fn if_empty<'a>(&'a self, default: &'a str) -> &'a str {
        if self.is_empty() { default } else { self }
    }
}
