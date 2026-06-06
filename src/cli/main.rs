#[path = "../config.rs"]
mod config;
#[path = "../clob.rs"]
mod clob;
#[path = "../feeds/mod.rs"]
mod feeds;
#[path = "../position.rs"]
mod position;
#[path = "../state.rs"]
mod state;
#[path = "../executor.rs"]
mod executor;
#[path = "../recorder.rs"]
mod recorder;
#[path = "../ws.rs"]
mod ws;
#[path = "../zscore.rs"]
mod zscore;
#[path = "../model.rs"]
mod model;
#[path = "../strategy/mod.rs"]
mod strategy;

use anyhow::Result;
use console::style;
use dialoguer::{theme::ColorfulTheme, Confirm, Input, Password, Select};
use std::path::PathBuf;

const SERVICE: &str = "jy-bot";

/// .env 路径（缓存解析结果）。优先级：
/// JY_ENV_PATH 环境变量 → 从 systemd 单元 ExecStart 解析 bot 实际启动用的 .env
/// （任何安装目录都对得上）→ config::default_env_path() 探测常见布局。
/// 不再写死 /opt/polymarket-copy。
fn env_path() -> &'static str {
    use std::sync::OnceLock;
    static P: OnceLock<String> = OnceLock::new();
    P.get_or_init(|| {
        if let Ok(p) = std::env::var("JY_ENV_PATH") {
            if !p.trim().is_empty() {
                return p.trim().to_string();
            }
        }
        if let Some(p) = env_path_from_systemd() {
            if std::path::Path::new(&p).exists() {
                return p;
            }
        }
        config::default_env_path()
    })
    .as_str()
}

/// 从 systemd 单元的 ExecStart 解析出 bot 启动时带的 .env 路径（取以 .env 结尾的参数）。
/// 这保证 CLI 读的 state 目录与 bot 写的完全一致，不依赖固定安装路径。
fn env_path_from_systemd() -> Option<String> {
    let out = std::process::Command::new("systemctl")
        .args(["show", SERVICE, "-p", "ExecStart", "--value"])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    s.split(|c: char| c.is_whitespace() || c == ';' || c == '{' || c == '}')
        .map(|t| t.trim())
        .find(|t| t.ends_with(".env"))
        .map(|t| t.to_string())
}

fn theme() -> ColorfulTheme {
    ColorfulTheme::default()
}

fn main() -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
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
        "stats" => match args.get(1).map(|s| s.as_str()) {
            Some("--ideal") => show_stats_file(&ideal_state_path()),
            Some("--diff")  => show_stats_diff(),
            _               => show_stats_file(&state_file_path()),
        },
        "clear" => clear_sim_data()?,
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
        "set-entry-strategy" => {
            let val = args.get(1).map(|s| s.as_str()).unwrap_or("");
            let v = match val.to_lowercase().as_str() {
                "zscore" | "z" => "zscore",
                "ev_solo" | "ev" | "solo" => "ev_solo",
                "sniper" | "snipe" | "s" => "sniper",
                "accum" | "ac" | "a" => "accum",
                other => {
                    eprintln!("未知入场策略: {other}（可选 zscore | ev_solo | sniper | accum）");
                    std::process::exit(1);
                }
            };
            set_env_val("ENTRY_STRATEGY", v);
            println!("{} ENTRY_STRATEGY={v}", style("✔").green());
            if args.contains(&"--restart".to_string()) {
                service_cmd("restart");
            }
        }
        "set-param" => {
            // 通用：jy set-param <KEY> <VALUE> [--restart]
            let key = args.get(1).map(|s| s.as_str()).unwrap_or("");
            let val = args.get(2).map(|s| s.as_str()).unwrap_or("");
            if key.is_empty() || val.is_empty() {
                eprintln!("用法: jy set-param <KEY> <VALUE> [--restart]");
                eprintln!("常用键: {}", TUNABLE_PARAMS.iter().map(|p| p.0).collect::<Vec<_>>().join(" "));
                std::process::exit(1);
            }
            set_env_val(key, val);
            println!("{} {key}={val}", style("✔").green());
            if args.contains(&"--restart".to_string()) {
                service_cmd("restart");
            }
        }
        "params" => show_params(),
        "update" => update_bot()?,
        _ => {
            eprintln!("未知命令: {}", args[0]);
            std::process::exit(1);
        }
    }
    Ok(())
}

/// 可调策略参数表：(env 键, 中文说明, 默认值)。用于 `jy params` 展示与交互式调参菜单。
const TUNABLE_PARAMS: &[(&str, &str, &str)] = &[
    ("TAKER_BUFFER",          "taker 吃单滑点缓冲(限价=ask+此值)",  "0.02"),
    ("EV_SOLO_MAX_ASK",       "ev_solo 入场价上限(≤才买)",         "0.52"),
    ("EV_SOLO_MIN_ASK",       "ev_solo 入场价下限(≥才买)",         "0.44"),
    ("EV_SOLO_QTY",           "每笔下单份数(与QUANT_ORDER_SHARES同步)", "20"),
    ("EV_SOLO_MIN_SECONDS_LEFT","ev_solo 只打开盘段(剩余≥此秒才入)", "240"),
    ("EV_SOLO_FLOW_GATE",     "ev_solo 资金流同向闸(1开/0关)",      "1"),
    ("SNIPER_MOVE_USD",       "sniper BTC突破阈值(美元,相对开盘动够才入)", "20"),
    ("SNIPER_MAX_ASK",        "sniper 限价闸(仅ask<此值才买,策略灵魂)", "0.62"),
    ("SNIPER_WINDOW_SEC",     "sniper 速度窗口(0=相对开盘累计;5=5秒内变动)", "0"),
    ("SNIPER_QTY",            "sniper 每笔下单份数(与其它份数键同步)", "20"),
    ("FORCE_LOCK_SECONDS_LEFT","强制处理线(剩余 ≤ 此秒触发)",       "60"),
    ("QUANT_ORDER_SHARES",    "每笔下单份数(与EV_SOLO_QTY同步)",   "20"),
    ("TREND_CHASE_MAX_PRICE", "zscore 追单价格上限",               "0.60"),
    ("LOCK_MIN_PROFIT_FACTOR","zscore 锁利门槛系数(×份额)",        "0.2"),
    ("ACCUM_ENTRY_Z",         "accum 首笔定方向阈值(|z|≥才开主腿)", "0.15"),
    ("ACCUM_CHASE_LEVELS",    "accum 追涨档(逗号分隔,谁涨追谁)",    "0.62,0.65,0.68,0.70"),
    ("ACCUM_DIP_LEVELS",      "accum 补仓档(逗号分隔,谁跌补谁)",    "0.25,0.20"),
    ("ACCUM_TARGET_WIN",      "accum 主腿赢目标PnL(补到≥此值)",    "12"),
    ("ACCUM_MAX_LOSS",        "accum 主腿输最大亏损(补到≥−此值)",  "7"),
    ("ACCUM_FORCE_SECONDS",   "accum 临结算停建线(剩余≤此秒不下单)", "15"),
];

/// 打印所有可调参数的当前值（未设置则显示默认）。
fn show_params() {
    println!("{}", style("── 策略可调参数 ──").bold());
    for (key, desc, default) in TUNABLE_PARAMS {
        let val = read_env_val(key).unwrap_or_else(|| format!("{default}(默认)"));
        println!("  {:<26} = {:<14} {}", style(key).dim(), style(val).white(), style(desc).dim());
    }
    println!("\n  改单个: {}", style("jy set-param <KEY> <VALUE> [--restart]").cyan());
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
        let entry = read_env_val("ENTRY_STRATEGY").unwrap_or_else(|| "zscore".into());
        println!(
            "  服务: {}  入场: {}  DRY_RUN: {}",
            if running { style("运行中").green().bold() } else { style("已停止").red().bold() },
            style(&entry).yellow(),
            if dry_run == "0" { style("实盘").red().bold() } else { style("模拟").green() },
        );
        println!();

        let items = vec![
            "1.  初始化/修改配置",
            "2.  查看当前配置",
            "3.  测试 API 连接",
            "4.  交易统计表",
            "5.  启动服务",
            "6.  停止服务",
            "7.  重启服务",
            "8.  查看实时日志",
            "9.  切换 DRY_RUN 模式",
            "10. 切换入场策略（zscore / ev_solo / sniper / accum）",
            "11. 调策略参数（sniper / ev_solo / 滑点 等）",
            "12. 清空模拟数据",
            "13. 更新程序（从 GitHub 拉取最新版本）",
            "0.  退出",
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
            3 => show_stats_menu(),
            4 => service_cmd("start"),
            5 => service_cmd("stop"),
            6 => service_cmd("restart"),
            7 => {
                show_logs(30);
                println!("\n{}", style("（按 Ctrl+C 停止日志）").dim());
                live_logs();
            }
            8 => toggle_dry_run()?,
            9 => toggle_entry_strategy()?,
            10 => tune_params()?,
            11 => clear_sim_data()?,
            12 => update_bot()?,
            13 => break,
            _ => {}
        }
    }
    Ok(())
}

fn edit_config() -> Result<()> {
    println!("{}", style("── 初始化/修改配置 ──").bold());

    let curr = |key: &str| read_env_val(key).unwrap_or_default();

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

    let order_shares: String = Input::with_theme(&theme())
        .with_prompt("每笔下单份数（实盘小额测试建议先填 1）")
        .default(curr("EV_SOLO_QTY").if_empty("20").into())
        .interact_text()?;

    // 写入 .env
    if !private_key.is_empty() { set_env_val("PRIVATE_KEY", &private_key); }
    if !deposit_wallet.is_empty() { set_env_val("DEPOSIT_WALLET_ADDRESS", &deposit_wallet); }
    set_env_val("DRY_RUN", if dry_run_choice == 0 { "1" } else { "0" });
    set_env_val("EV_SOLO_QTY", &order_shares); // 经 set_env_val 自动同步 QUANT_ORDER_SHARES

    println!("{} 配置已保存到 {}", style("✔").green(), env_path());
    Ok(())
}

fn show_config() {
    println!("{}", style("── 当前配置 ──").bold());
    let keys = [
        "ENTRY_STRATEGY", "DRY_RUN",
        "DEPOSIT_WALLET_ADDRESS",
        "ENTRY_STRATEGY", "EV_SOLO_QTY", "QUANT_ORDER_SHARES", "SIGNATURE_TYPE",
        "CLOB_API_URL", "CLOB_V2_API_URL",
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
        let cfg = config::load(Some(env_path())).unwrap();
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

fn toggle_entry_strategy() -> Result<()> {
    let curr = read_env_val("ENTRY_STRATEGY").unwrap_or_else(|| "zscore".into());
    let choice = Select::with_theme(&theme())
        .with_prompt("选择入场策略")
        .items(&[
            "zscore  - z-score 方向入场 + 锁利/追单/减险（baseline）",
            "ev_solo - z-score 定方向 + 纯单边裸持（开盘预测,已被sniper取代）",
            "sniper  - 延迟套利狙击:binance突破→抢盘口反应延迟（30天回测+5.7%,推荐）",
            "accum   - z定主腿→谁涨追谁/谁跌补谁(计算模块)→赢≥12/亏≤7锁住即停",
        ])
        .default(match curr.as_str() { "accum" => 3, "sniper" => 2, "ev_solo" => 1, _ => 0 })
        .interact()?;
    let new_val = match choice { 3 => "accum", 2 => "sniper", 1 => "ev_solo", _ => "zscore" };
    set_env_val("ENTRY_STRATEGY", new_val);
    println!("{} ENTRY_STRATEGY={new_val}", style("✔").green());
    if Confirm::with_theme(&theme()).with_prompt("重启服务？").default(true).interact()? {
        service_cmd("restart");
    }
    Ok(())
}

/// 交互式调参：列出可调参数，选一个改值，可选重启。
fn tune_params() -> Result<()> {
    println!("{}", style("── 调策略参数 ──").bold());
    let labels: Vec<String> = TUNABLE_PARAMS.iter().map(|(key, desc, default)| {
        let val = read_env_val(key).unwrap_or_else(|| format!("{default}(默认)"));
        format!("{key:<26} = {val:<14} {desc}")
    }).collect();
    let mut items: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();
    items.push("返回");
    let sel = Select::with_theme(&theme())
        .with_prompt("选择要修改的参数")
        .items(&items)
        .default(items.len() - 1)
        .interact()?;
    if sel >= TUNABLE_PARAMS.len() { return Ok(()); }

    let (key, desc, default) = TUNABLE_PARAMS[sel];
    let cur = read_env_val(key).unwrap_or_else(|| default.to_string());
    let new_val: String = Input::with_theme(&theme())
        .with_prompt(format!("{key}（{desc}）"))
        .default(cur)
        .interact_text()?;
    set_env_val(key, new_val.trim());
    println!("{} {key}={}", style("✔").green(), new_val.trim());
    if Confirm::with_theme(&theme()).with_prompt("重启服务使其生效？").default(true).interact()? {
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

// ── 交易统计 ──────────────────────────────────────────────────────────────

fn state_file_path() -> PathBuf {
    let f = read_env_val("QUANT_STATE_FILE").unwrap_or_else(|| "quant_state.json".into());
    let p = PathBuf::from(&f);
    if p.is_absolute() {
        p
    } else {
        PathBuf::from(env_path())
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join(f)
    }
}

/// signal 文件路径（扑空率/下单事件统计用）：默认 data/quant_signals.jsonl（相对 .env 目录）。
fn signal_file_path() -> PathBuf {
    let f = read_env_val("QUANT_SIGNAL_FILE").unwrap_or_else(|| "data/quant_signals.jsonl".into());
    let p = PathBuf::from(&f);
    if p.is_absolute() {
        p
    } else {
        PathBuf::from(env_path())
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join(f)
    }
}

/// 统计实盘扑空率（下单未成交率）：扫 signals.jsonl，只看实盘记录（dry_run=false），
/// phase=="miss" 计未成交，其余下单（非 settlement/entry_signal）计已成交。
/// 返回 (未成交数, 已成交数, 扑空率%)。无实盘下单记录则返回 None。
fn compute_miss_rate() -> Option<(usize, usize, f64)> {
    let text = std::fs::read_to_string(signal_file_path()).ok()?;
    let (mut miss, mut filled) = (0usize, 0usize);
    for line in text.lines() {
        let o: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        // 只统计实盘记录（成交单与 miss 单都带 dry_run=false）；无此字段或 true 一律跳过。
        if o.get("dry_run").and_then(|v| v.as_bool()) != Some(false) {
            continue;
        }
        match o.get("phase").and_then(|v| v.as_str()).unwrap_or("") {
            "miss" => miss += 1,
            "settlement" | "entry_signal" => {}
            _ => filled += 1,
        }
    }
    let total = miss + filled;
    if total == 0 {
        None
    } else {
        Some((miss, filled, miss as f64 / total as f64 * 100.0))
    }
}

/// 北京时间 HH:MM（按 UTC 时间戳 +8h）
fn bj_hm(ts: i64) -> String {
    let bj = ts + 8 * 3600;
    format!("{:02}:{:02}", (bj / 3600) % 24, (bj / 60) % 60)
}

/// 显示宽度：CJK/全角算 2，其余算 1
fn dwidth(s: &str) -> usize {
    s.chars().map(|c| if (c as u32) >= 0x2E80 { 2 } else { 1 }).sum()
}

fn pad(s: &str, w: usize) -> String {
    let d = dwidth(s);
    if d >= w { s.to_string() } else { format!("{}{}", s, " ".repeat(w - d)) }
}

/// 渲染分组表格：每个盘口一组，组与组之间用横线隔开（box-drawing 美化）。
fn render_grouped_table(headers: &[&str], groups: &[Vec<Vec<String>>]) {
    let cols = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| dwidth(h)).collect();
    for g in groups {
        for r in g {
            for (i, c) in r.iter().enumerate().take(cols) {
                widths[i] = widths[i].max(dwidth(c));
            }
        }
    }
    let rule = |l: &str, m: &str, r: &str| -> String {
        format!("{l}{}{r}", widths.iter().map(|w| "─".repeat(w + 2)).collect::<Vec<_>>().join(m))
    };
    let line = |cells: &[String]| -> String {
        let mut out = String::from("│");
        for (i, w) in widths.iter().enumerate() {
            let c = cells.get(i).map(|s| s.as_str()).unwrap_or("");
            out.push_str(&format!(" {} │", pad(c, *w)));
        }
        out
    };

    println!("{}", rule("┌", "┬", "┐"));
    println!("{}", line(&headers.iter().map(|h| h.to_string()).collect::<Vec<_>>()));
    println!("{}", rule("├", "┼", "┤"));
    for (gi, g) in groups.iter().enumerate() {
        if gi > 0 {
            println!("{}", rule("├", "┼", "┤"));
        }
        for r in g {
            println!("{}", line(r));
        }
    }
    println!("{}", rule("└", "┴", "┘"));
}

/// 交易阶段 → 中文短标
fn phase_label(p: &str) -> &'static str {
    match p {
        "entry" => "入场",
        "trend_chase" => "追单",
        "hedge" => "减险",
        "lock_profit" => "锁利",
        "lock_loss" => "锁亏",
        "lottery" => "彩票",
        "arb_entry" => "套利",
        "arb_lock" => "套利锁",
        _ => "其他",
    }
}

/// 影子账路径：quant_state.json → quant_state_ideal.json
fn ideal_state_path() -> std::path::PathBuf {
    let p = state_file_path();
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("quant_state");
    let ext = p.extension().and_then(|s| s.to_str()).unwrap_or("json");
    let parent = p.parent().unwrap_or_else(|| std::path::Path::new("."));
    parent.join(format!("{stem}_ideal.{ext}"))
}

/// 菜单入口：实盘有影子账时提示可用 --ideal/--diff，默认显示真实账
fn show_stats_menu() {
    let ideal = ideal_state_path();
    if ideal.exists() && std::fs::metadata(&ideal).map(|m| m.len() > 5).unwrap_or(false) {
        println!("{}", style("（实盘双轨：jy stats=真实账  jy stats --ideal=理想账  jy stats --diff=对比）").dim());
    }
    show_stats_file(&state_file_path());
}

/// 读取某状态文件的汇总（盘数/胜负/净盈亏）
fn read_totals(path: &std::path::Path) -> Option<(usize, usize, usize, f64)> {
    let text = std::fs::read_to_string(path).ok()?;
    let map: std::collections::HashMap<String, position::MarketPosition> =
        serde_json::from_str(&text).ok()?;
    let (mut n, mut win, mut lose, mut pnl) = (0, 0, 0, 0.0f64);
    for p in map.values() {
        if let Some(v) = p.realized_pnl {
            n += 1; pnl += v;
            if v >= 0.0 { win += 1 } else { lose += 1 }
        }
    }
    Some((n, win, lose, pnl))
}

/// 对比真实账 vs 理想账，量化滑点/未成交的代价
fn show_stats_diff() {
    let real = read_totals(&state_file_path());
    let ideal = read_totals(&ideal_state_path());
    match (real, ideal) {
        (Some((rn, rw, rl, rp)), Some((_in, iw, il, ip))) => {
            println!("{}", style("── 双轨对比（真实 vs 理想）──").bold());
            println!("  理想账(假设全额按ask成交)  结算{:3}盘  胜{}/负{}  净 ${:+.2}", _in, iw, il, ip);
            println!("  真实账(实际成交价/份额)    结算{:3}盘  胜{}/负{}  净 ${:+.2}", rn, rw, rl, rp);
            let gap = rp - ip;
            let g = if gap >= 0.0 { style(format!("${gap:+.2}")).green() } else { style(format!("${gap:+.2}")).red() };
            println!("  ── 现实代价(真实−理想): {g}  ←滑点+未成交+手续费差");
        }
        (Some(_), None) => println!("{} 暂无理想账（仅实盘模式才生成影子账）", style("ℹ").cyan()),
        _ => println!("{} 暂无数据", style("ℹ").cyan()),
    }
}

fn show_stats_file(path: &std::path::Path) {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => { println!("{} 暂无数据文件 {}", style("ℹ").cyan(), path.display()); return; }
    };
    let positions: std::collections::HashMap<String, position::MarketPosition> =
        match serde_json::from_str(&text) {
            Ok(p) => p,
            Err(e) => { println!("{} 解析失败: {e}", style("✖").red()); return; }
        };

    // 按盘口开始时间排序
    let mut markets: Vec<&position::MarketPosition> =
        positions.values().filter(|p| !p.trades.is_empty()).collect();
    markets.sort_by_key(|p| p.end_ts);

    if markets.is_empty() {
        println!("{} 暂无交易记录", style("ℹ").cyan());
        return;
    }

    let headers = ["盘口时间", "秒", "方向", "价格", "份额", "成本", "阶段", "结果", "盈亏"];
    let mut groups: Vec<Vec<Vec<String>>> = Vec::new();

    let (mut total, mut win, mut lose, mut locked, mut holding) = (0, 0, 0, 0, 0);
    let mut net_pnl = 0.0f64;

    for m in &markets {
        let start_ts = m.end_ts - 300;
        let label = format!("{}~{}", bj_hm(start_ts), bj_hm(m.end_ts));
        let n = m.trades.len();

        match m.realized_pnl {
            Some(p) => { total += 1; net_pnl += p; if p >= 0.0 { win += 1 } else { lose += 1 } }
            None => match format!("{:?}", m.phase).as_str() {
                "Locked" => locked += 1,
                _ => holding += 1,
            },
        }

        let mut group: Vec<Vec<String>> = Vec::new();
        for (i, t) in m.trades.iter().enumerate() {
            let is_last = i == n - 1;
            let (result, pnl) = if is_last {
                let r = m.winner.clone().unwrap_or_else(|| match format!("{:?}", m.phase).as_str() {
                    "Locked" => "锁定中".into(),
                    _ => "持仓中".into(),
                });
                let p = m.realized_pnl.map(|v| format!("{v:+.2}")).unwrap_or_else(|| "-".into());
                (r, p)
            } else {
                (String::new(), String::new())
            };
            group.push(vec![
                if i == 0 { label.clone() } else { String::new() },
                (t.ts - start_ts).to_string(),
                t.side.to_lowercase(),
                format!("{:.3}", t.price),
                format!("{:.0}", t.shares),
                format!("{:.2}", t.total_cost),
                phase_label(&t.phase).to_string(),
                result,
                pnl,
            ]);
        }
        groups.push(group);
    }

    render_grouped_table(&headers, &groups);
    let win_rate = if win + lose > 0 {
        win as f64 / (win + lose) as f64 * 100.0
    } else {
        0.0
    };
    println!(
        "\n  已结算 {} 盘  胜 {} / 负 {}  胜率 {:.1}%   锁定中 {}  持仓中 {}",
        total, win, lose, win_rate, locked, holding
    );
    let pnl_styled = if net_pnl >= 0.0 {
        style(format!("${net_pnl:+.2}")).green().bold()
    } else {
        style(format!("${net_pnl:+.2}")).red().bold()
    };
    println!("  已实现净盈亏: {pnl_styled}");
    // 实盘扑空率（下单未成交率）：仅当 signals 里有实盘下单记录时显示。
    if let Some((miss, filled, rate)) = compute_miss_rate() {
        let styled = if rate <= 20.0 {
            style(format!("{rate:.1}%")).green()
        } else {
            style(format!("{rate:.1}%")).yellow()
        };
        println!("  实盘扑空率: {styled}  (未成交 {} / 已发单 {})", miss, miss + filled);
    }
}

fn clear_sim_data() -> Result<()> {
    let path = state_file_path();
    if !Confirm::with_theme(&theme())
        .with_prompt(format!("确认清空模拟数据 {}？", path.display()))
        .default(false)
        .interact()?
    {
        println!("{} 已取消", style("✖").yellow());
        return Ok(());
    }
    // 必须先停服务：运行中的 bot 会把内存里的旧持仓 save() 回去，导致清空被覆盖
    let was_running = is_service_running();
    if was_running {
        service_cmd("stop");
    }
    std::fs::write(&path, "{}\n")?;
    println!("{} 已清空 {}", style("✔").green(), path.display());
    // 同时清空影子账（实盘双轨）
    let ideal = ideal_state_path();
    if ideal.exists() {
        std::fs::write(&ideal, "{}\n")?;
        println!("{} 已清空 {}", style("✔").green(), ideal.display());
    }
    if was_running {
        service_cmd("start");
        println!("{} 服务已重新启动，从空状态开始", style("✔").green());
    }
    Ok(())
}

fn read_env_val(key: &str) -> Option<String> {
    let path = env_path();
    if std::path::Path::new(path).exists() {
        if let Ok(content) = std::fs::read_to_string(path) {
            for line in content.lines() {
                if let Some(rest) = line.strip_prefix(&format!("{key}=")) {
                    return Some(rest.trim().to_string());
                }
            }
        }
    }
    None
}

fn set_env_raw(key: &str, val: &str) {
    let path = PathBuf::from(env_path());
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

/// 写入 .env。CLI 是配置的唯一权威:在此处把"份数"的两个 key
/// (EV_SOLO_QTY / QUANT_ORDER_SHARES)始终同步为同值,保证不管当前用哪个策略,
/// "CLI 设多少、系统实际就下多少",绝不再出现设一个 key、系统用另一个的脱节。
/// 所有 CLI 写入路径(setup 向导 / set-param / 交互菜单)都经过本函数,一处覆盖全部。
fn set_env_val(key: &str, val: &str) {
    set_env_raw(key, val);
    // 份数四键(EV_SOLO_QTY / QUANT_ORDER_SHARES / SNIPER_QTY / ACCUM_QTY)始终同步为同值,
    // 保证不管当前用哪个策略,CLI 设多少系统就下多少。
    match key {
        "EV_SOLO_QTY" | "QUANT_ORDER_SHARES" | "SNIPER_QTY" | "ACCUM_QTY" => {
            for k in ["EV_SOLO_QTY", "QUANT_ORDER_SHARES", "SNIPER_QTY", "ACCUM_QTY"] {
                if k != key { set_env_raw(k, val); }
            }
        }
        _ => {}
    }
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
    // 原子替换:cp 到 .new 临时文件 → mv -f 覆盖。
    // 关键:update 是用正在运行的 jy 跑的,直接 cp 覆盖 /usr/local/bin/jy 会
    // "Text file busy"。mv(rename)不截断目标,能替换正在运行的二进制,彻底避坑。
    let ok_bot = install_bin(&format!("{bin_dir}/jy-bot"), "/usr/local/bin/jy-bot");
    let ok_cli = install_bin(&format!("{bin_dir}/jy"), "/usr/local/bin/jy");
    if !ok_bot || !ok_cli {
        println!("  {} 二进制安装失败,服务仍用旧版本重启,请人工检查", style("✖").red());
        let _ = std::process::Command::new("sudo")
            .args(["systemctl", "start", SERVICE])
            .status();
        return Ok(());
    }
    println!("  {} 二进制已原子替换", style("✔").green());

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

/// 原子安装一个二进制:sudo cp src→dst.new,再 sudo mv -f dst.new→dst。
/// 用 mv(rename)而非直接 cp,以替换"正在运行"的二进制(避免 Text file busy)。
/// 任一步失败返回 false(不再静默吞错)。
fn install_bin(src: &str, dst: &str) -> bool {
    let tmp = format!("{dst}.new");
    let copied = std::process::Command::new("sudo")
        .args(["cp", src, &tmp]).status();
    if !matches!(copied, Ok(s) if s.success()) {
        println!("  {} 复制失败: {src} → {tmp}", style("✖").red());
        return false;
    }
    let moved = std::process::Command::new("sudo")
        .args(["mv", "-f", &tmp, dst]).status();
    if !matches!(moved, Ok(s) if s.success()) {
        println!("  {} 替换失败: {tmp} → {dst}", style("✖").red());
        let _ = std::process::Command::new("sudo").args(["rm", "-f", &tmp]).status();
        return false;
    }
    true
}

trait StrExt {
    fn if_empty<'a>(&'a self, default: &'a str) -> &'a str;
}
impl StrExt for str {
    fn if_empty<'a>(&'a self, default: &'a str) -> &'a str {
        if self.is_empty() { default } else { self }
    }
}
