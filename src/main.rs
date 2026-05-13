mod chain;
mod gpu;

use anyhow::{anyhow, Context, Result};
use chain::{ChainCtx, EpochSnapshot, SubmitOutcome};
use clap::Parser;
use gpu::{GpuMiner, MineHit};
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

#[derive(Parser, Debug)]
#[command(version, about = "hash256.org GPU 矿工 CLI")]
struct Cli {
    /// 配置文件路径
    #[arg(short, long, default_value = "config.toml")]
    config: PathBuf,
    /// 离线自检: 用低难度跑一个 batch, 校验 GPU shader 与 CPU keccak 一致
    #[arg(long)]
    selftest: bool,
    /// 强制重新跑 GPU benchmark, 忽略 config.toml 里锁定的值
    #[arg(long)]
    retune: bool,
}

#[derive(Debug, Deserialize)]
struct Config {
    private_key: Option<String>,
    contract: String,
    gpu: GpuConfig,
    rpc: RpcConfig,
    mining: MiningConfig,
    log: LogConfig,
}

#[derive(Debug, Deserialize)]
struct GpuConfig {
    // 单卡向后兼容字段 (会迁移到 profiles)
    #[serde(default)]
    batch_size: u64,
    #[serde(default)]
    pipeline_depth: u32,
    #[serde(default)]
    fingerprint: String,
    #[serde(default = "default_target_batch_ms")]
    target_batch_ms: f64,
    /// 多卡: 按 GPU fingerprint 缓存调优结果, 同型号自动共享
    #[serde(default)]
    profiles: std::collections::HashMap<String, GpuProfile>,
}

#[derive(Debug, Deserialize, Clone)]
struct GpuProfile {
    batch_size: u64,
    pipeline_depth: u32,
}

#[derive(Debug, Deserialize)]
struct RpcConfig {
    read: String,
    submit: String,
}

#[derive(Debug, Deserialize)]
struct MiningConfig {
    /// "market" = 跟随市场分位; "fixed" = 用 tip_gwei
    #[serde(default = "default_tip_mode")]
    tip_mode: String,
    /// market 模式下取的市场实付 tip 分位 (10/25/50/75/90)
    #[serde(default = "default_tip_percentile")]
    tip_percentile: f64,
    /// 市场极冷或 oracle 没数据时的下限 (gwei)
    #[serde(default = "default_tip_floor")]
    tip_floor_gwei: f64,
    /// fixed 模式 / oracle fallback
    tip_gwei: f64,
    /// 经济护栏: gas/reward 比超过 profit_ratio 就不发 tx, 继续挖等下次机会
    #[serde(default = "default_profit_guard")]
    pause_when_unprofitable: bool,
    /// gas/reward 比例上限. 0.5 = gas 不能超过奖励一半; 1.0 = gas 不能超过奖励
    #[serde(default = "default_profit_ratio")]
    profit_ratio: f64,
    wait_confirmations: u64,
    submit_timeout_secs: u64,
    #[serde(default = "default_watch_interval")]
    challenge_watch_secs: u64,
}
fn default_tip_mode() -> String { "market".to_string() }
fn default_tip_percentile() -> f64 { 75.0 }
fn default_tip_floor() -> f64 { 1.0 }
fn default_profit_guard() -> bool { true }
fn default_profit_ratio() -> f64 { 1.0 }

fn default_watch_interval() -> u64 {
    3
}
fn default_target_batch_ms() -> f64 {
    100.0
}

#[derive(Debug, Deserialize)]
struct LogConfig {
    level: String,
}

/// 解析 config.toml 的路径: 依次在 cwd、可执行文件目录、cwd/.. 找.
/// 找到第一个存在的就返回; 都不存在则返回 cwd 下原路径让后续报错带正确路径.
fn resolve_config_path(arg: &PathBuf) -> PathBuf {
    if arg.is_absolute() && arg.exists() {
        return arg.clone();
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    let candidates = [
        cwd.join(arg),
        cwd.join("..").join(arg),
        exe_dir.as_ref().map(|d| d.join(arg)).unwrap_or_default(),
        exe_dir.as_ref().map(|d| d.join("..").join(arg)).unwrap_or_default(),
    ];
    for c in candidates.iter() {
        if !c.as_os_str().is_empty() && c.exists() {
            return c.clone();
        }
    }
    cwd.join(arg)
}

fn load_config(path: &PathBuf) -> Result<(Config, PathBuf)> {
    let resolved = resolve_config_path(path);
    let raw = std::fs::read_to_string(&resolved)
        .with_context(|| format!("读取配置 {} 失败 (可从 config.example.toml 复制)", resolved.display()))?;
    Ok((toml::from_str(&raw)?, resolved))
}

/// 后台 oracle: base_fee / ETH-USD / HASH-USD / 市场 tip 分位.
/// 各任务自己控制频率, 写入共享 atomic.
fn spawn_oracle(
    chain: ChainCtx,
    base_fee_wei: Arc<AtomicU64>,
    eth_usd_x1000: Arc<AtomicU64>,
    hash_usd_x1e6: Arc<AtomicU64>,
    market_tip_wei: Arc<AtomicU64>,
    reward_hash_x1000: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    tip_percentile: f64,
) {
    // currentReward 5min (减半频率很低, 每 100k 次铸造 ≈ 70 天才变一次)
    {
        let chain = chain.clone();
        let r = reward_hash_x1000.clone();
        let stop = stop.clone();
        tokio::spawn(async move {
            loop {
                if stop.load(Ordering::SeqCst) { break; }
                if let Ok(v) = chain.fetch_reward_hash().await {
                    r.store((v * 1000.0) as u64, Ordering::Relaxed);
                }
                sleep(Duration::from_secs(300)).await;
            }
        });
    }
    // base_fee 6s
    {
        let chain = chain.clone();
        let bf = base_fee_wei.clone();
        let stop = stop.clone();
        tokio::spawn(async move {
            loop {
                if stop.load(Ordering::SeqCst) { break; }
                if let Ok(w) = chain.fetch_base_fee().await {
                    bf.store(w as u64, Ordering::Relaxed);
                }
                sleep(Duration::from_secs(6)).await;
            }
        });
    }
    // ETH/USD 30s
    {
        let chain = chain.clone();
        let p = eth_usd_x1000.clone();
        let stop = stop.clone();
        tokio::spawn(async move {
            loop {
                if stop.load(Ordering::SeqCst) { break; }
                if let Ok(u) = chain.fetch_eth_usd().await {
                    p.store((u * 1000.0) as u64, Ordering::Relaxed);
                }
                sleep(Duration::from_secs(30)).await;
            }
        });
    }
    // HASH/USD 60s (DEX Screener)
    {
        let chain = chain.clone();
        let p = hash_usd_x1e6.clone();
        let stop = stop.clone();
        tokio::spawn(async move {
            loop {
                if stop.load(Ordering::SeqCst) { break; }
                match chain.fetch_hash_usd().await {
                    Ok(u) => p.store((u * 1_000_000.0) as u64, Ordering::Relaxed),
                    Err(e) => eprintln!("[价格] HASH/USD 拉取失败: {e:#}"),
                }
                sleep(Duration::from_secs(60)).await;
            }
        });
    }
    // 市场 tip 20s (扫最近 8 块成功 mine() tx 的实付 priority fee 分位)
    {
        let chain = chain.clone();
        let t = market_tip_wei.clone();
        let stop = stop.clone();
        tokio::spawn(async move {
            loop {
                if stop.load(Ordering::SeqCst) { break; }
                match chain.fetch_market_tip(4, tip_percentile).await {
                    Ok(Some(w)) => t.store(w as u64, Ordering::Relaxed),
                    Ok(None) => {} // 没样本, 保留旧值
                    Err(e) => eprintln!("[市场 tip] 拉取失败: {e:#}"),
                }
                sleep(Duration::from_secs(20)).await;
            }
        });
    }
}

/// 假设 mine() 消耗 100k gas, 算预估 (ETH, USD).
fn estimate_cost(base_fee_wei: u64, tip_gwei: f64, eth_usd: f64) -> (f64, f64) {
    let tip_wei = (tip_gwei * 1e9) as u64;
    let total_wei_per_gas = base_fee_wei.saturating_add(tip_wei) as f64;
    let gas_used = 100_000.0_f64;
    let eth = total_wei_per_gas * gas_used / 1e18;
    let usd = eth * eth_usd;
    (eth, usd)
}

/// 返回当前应该用的 tip (gwei). market 模式跟随 oracle 但不低于 floor.
fn current_tip_gwei(
    tip_mode: &str,
    fallback_gwei: f64,
    floor_gwei: f64,
    market_tip_wei: &AtomicU64,
) -> f64 {
    if tip_mode == "market" {
        let w = market_tip_wei.load(Ordering::Relaxed);
        if w > 0 {
            let g = w as f64 / 1e9;
            return g.max(floor_gwei);
        }
        // 没数据用 fallback
        fallback_gwei.max(floor_gwei)
    } else {
        fallback_gwei
    }
}

/// 经济判断: gas_cost / reward_usd >= profit_ratio 返回 false (不发).
/// hash_usd_x1e6 = 0 或 eth_usd_x1000 = 0 时视为不发 (oracle 还没就绪).
fn is_profitable(
    base_fee_wei: u64,
    tip_gwei: f64,
    eth_usd: f64,
    hash_usd: f64,
    reward_hash: f64,
    profit_ratio: f64,
) -> (bool, f64, f64) {
    let (_, cost_usd) = estimate_cost(base_fee_wei, tip_gwei, eth_usd);
    let reward_usd = reward_hash * hash_usd;
    if reward_usd <= 0.0 {
        return (false, cost_usd, reward_usd);
    }
    let ratio = cost_usd / reward_usd;
    (ratio < profit_ratio, cost_usd, reward_usd)
}

/// 多卡: 把 profiles 字典写回 [gpu].profiles 段, 同 fingerprint 共享.
fn persist_profiles(
    path: &PathBuf,
    profiles: &std::collections::HashMap<String, GpuProfile>,
) -> Result<()> {
    let raw = std::fs::read_to_string(path)?;
    let mut doc: toml::Value = toml::from_str(&raw)?;
    let gpu = doc
        .get_mut("gpu")
        .and_then(|v| v.as_table_mut())
        .ok_or_else(|| anyhow!("config.toml 缺少 [gpu] 段"))?;
    let mut profiles_table = toml::value::Table::new();
    for (fp, p) in profiles {
        let mut t = toml::value::Table::new();
        t.insert(
            "batch_size".into(),
            toml::Value::Integer(p.batch_size as i64),
        );
        t.insert(
            "pipeline_depth".into(),
            toml::Value::Integer(p.pipeline_depth as i64),
        );
        profiles_table.insert(fp.clone(), toml::Value::Table(t));
    }
    gpu.insert("profiles".into(), toml::Value::Table(profiles_table));
    // 清掉单卡旧字段, 避免下次读取时再迁移一遍
    gpu.remove("batch_size");
    gpu.remove("pipeline_depth");
    gpu.remove("fingerprint");
    let new = toml::to_string_pretty(&doc)?;
    std::fs::write(path, new)?;
    Ok(())
}

/// 单卡向后兼容版 (旧的, 现在用 persist_profiles 替代).
#[allow(dead_code)]
fn persist_tuning(
    path: &PathBuf,
    batch_size: u64,
    pipeline_depth: u32,
    fingerprint: &str,
) -> Result<()> {
    let raw = std::fs::read_to_string(path)?;
    let mut doc: toml::Value = toml::from_str(&raw)?;
    let gpu = doc
        .get_mut("gpu")
        .and_then(|v| v.as_table_mut())
        .ok_or_else(|| anyhow!("config.toml 缺少 [gpu] 段"))?;
    gpu.insert("batch_size".into(), toml::Value::Integer(batch_size as i64));
    gpu.insert(
        "pipeline_depth".into(),
        toml::Value::Integer(pipeline_depth as i64),
    );
    gpu.insert(
        "fingerprint".into(),
        toml::Value::String(fingerprint.to_string()),
    );
    let new = toml::to_string_pretty(&doc)?;
    std::fs::write(path, new)?;
    Ok(())
}

fn resolve_private_key(cfg: &Config) -> Result<String> {
    if let Some(pk) = &cfg.private_key {
        if !pk.trim().is_empty() {
            return Ok(pk.trim().to_string());
        }
    }
    // fallback: 找 .env. 跟 config.toml 同样的搜索策略.
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    let candidates: Vec<PathBuf> = vec![
        cwd.join(".env"),
        cwd.join("..").join(".env"),
        exe_dir.as_ref().map(|d| d.join(".env")).unwrap_or_default(),
        exe_dir.as_ref().map(|d| d.join("..").join(".env")).unwrap_or_default(),
    ];
    for p in candidates.iter() {
        if !p.as_os_str().is_empty() && p.exists() {
            let _ = dotenvy::from_path(p);
            break;
        }
    }
    std::env::var("PRIVATE_KEY")
        .map_err(|_| anyhow!("config.toml 里 private_key 为空, 且 .env 里没有 PRIVATE_KEY"))
}

fn print_banner(
    ctx: &ChainCtx,
    gpu: &GpuMiner,
    batch_size: u64,
    pipeline_depth: u32,
    tip_mode: &str,
    tip_percentile: f64,
    tip_floor: f64,
    profit_ratio: f64,
) {
    let bar = "─".repeat(58);
    println!("┌─ hash-miner v0.1.0 {}", bar);
    println!("│ 矿工地址  {}", ctx.miner);
    println!("│ 合约地址  {}", ctx.contract);
    println!("│ 读取 RPC  {}", short_url(&ctx.read_rpc));
    println!(
        "│ 提交 RPC  {}{}",
        short_url(&ctx.submit_rpc),
        if ctx.submit_rpc.contains("flashbots") || ctx.submit_rpc.contains("mevblocker") {
            " (私有 mempool)"
        } else {
            ""
        }
    );
    println!("│ GPU       {} · {}", gpu.adapter_name, gpu.backend);
    println!(
        "│ 批量      {} nonce · pipeline={}",
        batch_size, pipeline_depth
    );
    if tip_mode == "market" {
        println!(
            "│ 优先费    market 跟随 (取 {}% 分位, 下限 {} gwei)",
            tip_percentile, tip_floor
        );
    } else {
        println!("│ 优先费    固定 (fallback)");
    }
    println!(
        "│ 经济护栏  gas/reward 比 ≥ {} 时跳过 (持续挖, 不发 tx)",
        profit_ratio
    );
    println!("└{}{}", bar, "─".repeat(20));
    println!();
}

fn short_url(u: &str) -> String {
    u.strip_prefix("https://")
        .or_else(|| u.strip_prefix("http://"))
        .unwrap_or(u)
        .to_string()
}

fn print_snapshot(snap: &EpochSnapshot) {
    println!(
        "[链上] 挑战 {}",
        chain::format_hex_short(&snap.challenge)
    );
    println!(
        "[链上] 难度 {}",
        chain::format_hex_short(&snap.difficulty)
    );
    println!(
        "[链上] 单次奖励 {} HASH · 本纪元剩余 {} 块",
        chain::format_hash_18(snap.reward),
        snap.blocks_left
    );
}

/// Windows 控制台默认不解析 ANSI escape, 需要显式开 VT100 处理.
/// macOS / Linux 终端原生支持, 这个函数在那俩平台是空操作.
#[cfg(windows)]
fn enable_ansi_on_windows() {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
        STD_OUTPUT_HANDLE,
    };
    unsafe {
        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return;
        }
        let mut mode: u32 = 0;
        if GetConsoleMode(handle, &mut mode) != 0 {
            let _ = SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
        }
    }
}

#[cfg(not(windows))]
fn enable_ansi_on_windows() {}

#[tokio::main]
async fn main() -> Result<()> {
    enable_ansi_on_windows();
    let cli = Cli::parse();
    let (cfg, config_path) = load_config(&cli.config)?;

    // 过滤掉 wgpu 自己的 INFO 噪音, 只保留我们自己的日志
    let base = format!(
        "{},wgpu_core=warn,wgpu_hal=warn,naga=warn,alloy=warn,hyper=warn",
        cfg.log.level
    );
    let filter = tracing_subscriber::EnvFilter::try_new(&base)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,wgpu_core=warn,wgpu_hal=warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .without_time()
        .init();

    let pk = resolve_private_key(&cfg)?;
    let chain_ctx = ChainCtx::new(
        &pk,
        &cfg.contract,
        cfg.rpc.read.clone(),
        cfg.rpc.submit.clone(),
        cfg.mining.submit_timeout_secs,
        cfg.mining.wait_confirmations,
    )?;

    println!("[启动] 检测 GPU…");
    let gpus = GpuMiner::enumerate_all().await?;
    println!("[GPU]  检测到 {} 张可用 GPU:", gpus.len());
    for (i, g) in gpus.iter().enumerate() {
        println!("       [{}] {} · {}", i, g.adapter_name, g.backend);
    }

    if cli.selftest {
        // 自检用第一张卡
        return run_selftest(&gpus[0]).await;
    }

    // 决定每张卡的 batch_size / pipeline_depth.
    // 同 fingerprint 共享 profile, 第一次测的写回 config.
    let mut profiles: std::collections::HashMap<String, GpuProfile> = cfg.gpu.profiles.clone();
    // 单卡向后兼容: 旧字段 fingerprint+batch_size+pipeline_depth 迁移到 profiles
    if !cfg.gpu.fingerprint.is_empty() && cfg.gpu.batch_size > 0 && cfg.gpu.pipeline_depth > 0 {
        profiles
            .entry(cfg.gpu.fingerprint.clone())
            .or_insert(GpuProfile {
                batch_size: cfg.gpu.batch_size,
                pipeline_depth: cfg.gpu.pipeline_depth,
            });
    }

    let mut gpu_runtime: Vec<(Arc<GpuMiner>, u64, u32)> = Vec::new();
    let mut profile_updated = false;
    for (idx, gpu) in gpus.into_iter().enumerate() {
        let fp = gpu.fingerprint.clone();
        let need_tune = cli.retune || !profiles.contains_key(&fp);
        let (b, p) = if need_tune {
            if cli.retune {
                println!("[基准] [{}] --retune 强制重测 ({})", idx, gpu.adapter_name);
            } else {
                println!(
                    "[基准] [{}] {} 首次见到, 开始测算 (目标 ~{:.0} ms/batch)…",
                    idx, gpu.adapter_name, cfg.gpu.target_batch_ms
                );
            }
            let bench = gpu.auto_tune(cfg.gpu.target_batch_ms).await?;
            println!(
                "[基准] [{}] ✓ batch={}M · pipeline={} · ~{:.1} MH/s",
                idx,
                bench.batch_size / 1_000_000,
                bench.pipeline_depth,
                bench.hashrate_mhs
            );
            profiles.insert(
                fp.clone(),
                GpuProfile {
                    batch_size: bench.batch_size,
                    pipeline_depth: bench.pipeline_depth,
                },
            );
            profile_updated = true;
            (bench.batch_size, bench.pipeline_depth)
        } else {
            let p = profiles.get(&fp).expect("checked above");
            println!(
                "[配置] [{}] 复用 profile: batch={}M · pipeline={}",
                idx,
                p.batch_size / 1_000_000,
                p.pipeline_depth
            );
            (p.batch_size, p.pipeline_depth)
        };
        gpu_runtime.push((Arc::new(gpu), b, p));
    }

    if profile_updated {
        if let Err(e) = persist_profiles(&config_path, &profiles) {
            eprintln!("[基准] ⚠ 写回 config.toml 失败: {e:#}");
        } else {
            println!("[基准] 已写回 config.toml ({} 个 profile)", profiles.len());
        }
    }

    let n_cards = gpu_runtime.len();
    let primary_gpu = &gpu_runtime[0].0;
    let primary_batch = gpu_runtime[0].1;
    let primary_pipe = gpu_runtime[0].2;

    print_banner(
        &chain_ctx,
        primary_gpu,
        primary_batch,
        primary_pipe,
        &cfg.mining.tip_mode,
        cfg.mining.tip_percentile,
        cfg.mining.tip_floor_gwei,
        cfg.mining.profit_ratio,
    );

    // Ctrl+C 处理
    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            println!("\n[中断] 收到 Ctrl+C, 优雅退出…");
            stop.store(true, Ordering::SeqCst);
        });
    }

    let bal_before = chain_ctx.balance().await?;
    println!(
        "[余额] 当前 HASH 余额 {} HASH",
        chain::format_hash_18(bal_before)
    );

    // 初始化 CLI 自管的 account nonce 池
    match chain_ctx.init_nonce().await {
        Ok(n) => println!("[nonce] 同步链上 pending nonce = {}", n),
        Err(e) => eprintln!("[警告] 初始化 nonce 失败: {e:#} (会在首次发送时重试)"),
    }

    // 启动后台 oracle: base_fee / ETH-USD / HASH-USD / 市场 tip / 当前奖励
    let base_fee_wei = Arc::new(AtomicU64::new(0));
    let eth_usd_x1000 = Arc::new(AtomicU64::new(0));
    let hash_usd_x1e6 = Arc::new(AtomicU64::new(0));
    let market_tip_wei = Arc::new(AtomicU64::new(0));
    let reward_hash_x1000 = Arc::new(AtomicU64::new(0));
    spawn_oracle(
        chain_ctx.clone(),
        base_fee_wei.clone(),
        eth_usd_x1000.clone(),
        hash_usd_x1e6.clone(),
        market_tip_wei.clone(),
        reward_hash_x1000.clone(),
        stop.clone(),
        cfg.mining.tip_percentile,
    );
    // 等关键 oracle 初始化. base_fee/eth_usd 必须有;
    // hash_usd 必要(经济护栏依赖它), 但 DEX Screener 偶尔抽风, 等 10 秒不到就警告继续
    for _ in 0..100 {
        let ready = base_fee_wei.load(Ordering::Relaxed) > 0
            && eth_usd_x1000.load(Ordering::Relaxed) > 0
            && hash_usd_x1e6.load(Ordering::Relaxed) > 0
            && reward_hash_x1000.load(Ordering::Relaxed) > 0;
        if ready {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    if hash_usd_x1e6.load(Ordering::Relaxed) == 0 {
        eprintln!(
            "[警告] HASH/USD 价格未就绪 (DEX Screener 可能离线). \
             经济护栏会暂时拦截所有 tx, 直到价格拉到. 改 pause_when_unprofitable=false 可绕过"
        );
    }
    let bf = base_fee_wei.load(Ordering::Relaxed);
    let eth_usd = eth_usd_x1000.load(Ordering::Relaxed) as f64 / 1000.0;
    let hash_usd = hash_usd_x1e6.load(Ordering::Relaxed) as f64 / 1_000_000.0;
    let mkt_tip_gwei = market_tip_wei.load(Ordering::Relaxed) as f64 / 1e9;
    println!(
        "[行情] base_fee={:.3} gwei · ETH=${:.2} · HASH=${:.4} · 市场 tip {:.0}%={}",
        bf as f64 / 1e9,
        eth_usd,
        hash_usd,
        cfg.mining.tip_percentile,
        if mkt_tip_gwei > 0.0 {
            format!("{:.2} gwei", mkt_tip_gwei)
        } else {
            "(等待样本)".to_string()
        }
    );
    println!();

    let total_mined = Arc::new(AtomicU64::new(0));
    let total_failed = Arc::new(AtomicU64::new(0));
    // 进度行标志放外层, 跨纪元保留状态
    let printed_once = Arc::new(AtomicBool::new(false));
    // 帮手宏: 任何固定打印前先 detach 进度行
    let detach = |p: &Arc<AtomicBool>| {
        if p.swap(false, Ordering::Relaxed) {
            eprintln!();
        }
    };

    // 外层循环: 每次 challenge 变化 (纪元切换) 或启动时刷新 snapshot.
    // 内层循环: 同一 challenge 内连续命中, 不重读 snapshot, nonce 累加.
    'outer: while !stop.load(Ordering::SeqCst) {
        // 1. 拉一次 snapshot (启动或纪元切换后)
        let snap = match chain_ctx.snapshot().await {
            Ok(s) => s,
            Err(e) => {
                detach(&printed_once);
                eprintln!("[错误] 读取链上状态失败: {e:#}, 5 秒后重试");
                sleep(Duration::from_secs(5)).await;
                continue;
            }
        };
        detach(&printed_once);
        print_snapshot(&snap);

        let challenge_now = snap.challenge;
        let difficulty_now = snap.difficulty;

        // 2. challenge watcher: 同一 snapshot 周期内只启一次
        let chain_for_watch = chain_ctx.clone();
        let stop_flag = stop.clone();
        let challenge_changed = Arc::new(AtomicBool::new(false));
        let cc = challenge_changed.clone();
        let blocks_left_shared = Arc::new(AtomicU64::new(
            snap.blocks_left.try_into().unwrap_or(u64::MAX),
        ));
        let bl_for_watch = blocks_left_shared.clone();
        let watch_interval = cfg.mining.challenge_watch_secs;
        let watcher = tokio::spawn(async move {
            loop {
                sleep(Duration::from_secs(watch_interval)).await;
                if stop_flag.load(Ordering::SeqCst) || cc.load(Ordering::SeqCst) {
                    break;
                }
                if let Ok((cur, blocks)) = chain_for_watch.fetch_challenge_and_blocks_left().await {
                    bl_for_watch.store(blocks, Ordering::Relaxed);
                    if cur != challenge_now {
                        cc.store(true, Ordering::SeqCst);
                        break;
                    }
                }
            }
        });

        // 启动 GPU 任务: 每张卡用高 4 bit 作为 nonce 分片避免重复
        // 多卡每张的 hashrate 共享 (写 MH/s × 100, 即 0.01 MH/s 精度)
        let card_hashrate_x100: Vec<Arc<AtomicU64>> =
            (0..n_cards).map(|_| Arc::new(AtomicU64::new(0))).collect();
        let card_nonces: Vec<Arc<AtomicU64>> =
            (0..n_cards).map(|_| Arc::new(AtomicU64::new(0))).collect();

        // 每张卡的命中通过 mpsc 送回主循环
        let (hit_tx, mut hit_rx) = tokio::sync::mpsc::channel::<MineHit>(n_cards * 4);

        let mut card_tasks = Vec::new();
        for (idx, (gpu, b, p)) in gpu_runtime.iter().enumerate() {
            let gpu = gpu.clone();
            let b = *b;
            let p = *p;
            let hit_tx = hit_tx.clone();
            let stop_for_gpu = stop.clone();
            let cc_for_gpu = challenge_changed.clone();
            let hr_share = card_hashrate_x100[idx].clone();
            let nonce_share = card_nonces[idx].clone();
            let challenge_for_gpu = challenge_now;
            let difficulty_for_gpu = difficulty_now;
            // 分片: 高 4 bit = gpu 索引 (支持最多 16 张卡)
            let shard_high = (idx as u64) << 60;
            let seed_low = rand_seed() & 0x0fff_ffff_ffff_ffff;
            let start_nonce = shard_high | seed_low;
            println!(
                "[挖矿] [{}] 起始 nonce 0x{:016x} · 开始搜索…",
                idx, start_nonce
            );

            let task = tokio::spawn(async move {
                let mut cursor = start_nonce;
                loop {
                    if stop_for_gpu.load(Ordering::SeqCst)
                        || cc_for_gpu.load(Ordering::SeqCst)
                    {
                        return Ok::<(), anyhow::Error>(());
                    }
                    let stop_c = stop_for_gpu.clone();
                    let cc_c = cc_for_gpu.clone();
                    let hr_c = hr_share.clone();
                    let nc_c = nonce_share.clone();
                    let result = gpu
                        .search(
                            b,
                            p,
                            challenge_for_gpu,
                            difficulty_for_gpu,
                            cursor,
                            move || stop_c.load(Ordering::SeqCst) || cc_c.load(Ordering::SeqCst),
                            move |prog| {
                                hr_c.store(
                                    (prog.hashrate_mhs * 100.0) as u64,
                                    Ordering::Relaxed,
                                );
                                nc_c.store(prog.nonces_tried, Ordering::Relaxed);
                            },
                        )
                        .await?;
                    match result {
                        Some(hit) => {
                            // 防止越过自己分片边界 (虽然 2^60 范围超大基本不会撞)
                            cursor = hit.nonce.wrapping_add(1);
                            let _ = hit_tx.send(hit).await;
                        }
                        None => return Ok(()),
                    }
                }
            });
            card_tasks.push(task);
        }
        drop(hit_tx); // 主任务只收, 不送

        // 进度行后台任务: 聚合所有 GPU 算力, 1 秒打印一次
        let progress_stop = challenge_changed.clone();
        let progress_main_stop = stop.clone();
        let bl_for_progress = blocks_left_shared.clone();
        let bf_for_progress = base_fee_wei.clone();
        let usd_for_progress = eth_usd_x1000.clone();
        let hash_usd_for_progress = hash_usd_x1e6.clone();
        let mkt_tip_for_progress = market_tip_wei.clone();
        let reward_for_progress = reward_hash_x1000.clone();
        let tip_mode_for_progress = cfg.mining.tip_mode.clone();
        let tip_fallback = cfg.mining.tip_gwei;
        let tip_floor = cfg.mining.tip_floor_gwei;
        let printed_for_progress = printed_once.clone();
        let card_hr_for_progress = card_hashrate_x100.clone();
        let card_nonces_for_progress = card_nonces.clone();
        let progress_task = tokio::spawn(async move {
            loop {
                sleep(Duration::from_secs(1)).await;
                if progress_stop.load(Ordering::SeqCst)
                    || progress_main_stop.load(Ordering::SeqCst)
                {
                    break;
                }
                let total_mhs: f64 = card_hr_for_progress
                    .iter()
                    .map(|h| h.load(Ordering::Relaxed) as f64 / 100.0)
                    .sum();
                let total_nonces: u64 = card_nonces_for_progress
                    .iter()
                    .map(|n| n.load(Ordering::Relaxed))
                    .sum();
                let bl = bl_for_progress.load(Ordering::Relaxed);
                let bf = bf_for_progress.load(Ordering::Relaxed);
                let eth_usd = usd_for_progress.load(Ordering::Relaxed) as f64 / 1000.0;
                let hash_usd = hash_usd_for_progress.load(Ordering::Relaxed) as f64 / 1_000_000.0;
                let tip_g = current_tip_gwei(
                    &tip_mode_for_progress,
                    tip_fallback,
                    tip_floor,
                    &mkt_tip_for_progress,
                );
                let (eth_cost, usd_cost) = estimate_cost(bf, tip_g, eth_usd);
                let reward_hash =
                    reward_for_progress.load(Ordering::Relaxed) as f64 / 1000.0;
                let reward_usd = reward_hash * hash_usd;
                let net = reward_usd - usd_cost;
                let ratio = if reward_usd > 0.0 {
                    usd_cost / reward_usd * 100.0
                } else {
                    0.0
                };
                let bf_gwei = bf as f64 / 1e9;
                use std::io::Write;
                if printed_for_progress.swap(true, Ordering::Relaxed) {
                    eprint!("\x1b[2A");
                }
                let per_card: Vec<String> = card_hr_for_progress
                    .iter()
                    .enumerate()
                    .map(|(i, h)| {
                        format!("[{}]={:.1}", i, h.load(Ordering::Relaxed) as f64 / 100.0)
                    })
                    .collect();
                eprint!(
                    "\r\x1b[K[算力] 总 {:>6.1} MH/s · 已尝试 {:>5.2} G · 本纪元剩余 {} 块 · 奖励 {:.1} HASH · {}\n\
\r\x1b[K[行情] HASH=${:.4} · base={:.2} gwei · tip={:.2} gwei · gas={:.6} ETH (${:.2}) · 奖励 ${:.2} · 净 ${:+.2} ({:.0}%)\n",
                    total_mhs,
                    (total_nonces as f64) / 1e9,
                    bl,
                    reward_hash,
                    per_card.join(" "),
                    hash_usd,
                    bf_gwei,
                    tip_g,
                    eth_cost,
                    usd_cost,
                    reward_usd,
                    net,
                    ratio,
                );
                let _ = std::io::stderr().flush();
            }
        });

        // 主循环: 处理命中
        while let Some(hit) = hit_rx.recv().await {
            if stop.load(Ordering::SeqCst) || challenge_changed.load(Ordering::SeqCst) {
                break;
            }
            if printed_once.swap(false, Ordering::Relaxed) {
                eprintln!();
            }
            println!("★ 命中! nonce = 0x{:016x}", hit.nonce);
            println!("       hash = 0x{}", hex::encode(&hit.hash_be));

            if let Err(e) = verify_hit(&challenge_now, hit.nonce, &hit.hash_be, &difficulty_now) {
                eprintln!("[自检] 失败! 拒绝发交易: {e}");
                continue;
            }

            // 经济护栏: oracle 数据 + 当前应付 tip 算性价比
            let tip_now = current_tip_gwei(
                &cfg.mining.tip_mode,
                cfg.mining.tip_gwei,
                cfg.mining.tip_floor_gwei,
                &market_tip_wei,
            );
            let bf_now = base_fee_wei.load(Ordering::Relaxed);
            let eth_usd_now = eth_usd_x1000.load(Ordering::Relaxed) as f64 / 1000.0;
            let hash_usd_now = hash_usd_x1e6.load(Ordering::Relaxed) as f64 / 1_000_000.0;
            let reward_hash = reward_hash_x1000.load(Ordering::Relaxed) as f64 / 1000.0;
            let (ok, cost_usd, reward_usd) = is_profitable(
                bf_now,
                tip_now,
                eth_usd_now,
                hash_usd_now,
                reward_hash,
                cfg.mining.profit_ratio,
            );
            if cfg.mining.pause_when_unprofitable && !ok {
                eprintln!(
                    "[跳过] gas ${:.2} / 奖励 ${:.2} = {:.0}% ≥ {:.0}% · 不发, GPU 继续",
                    cost_usd,
                    reward_usd,
                    if reward_usd > 0.0 { cost_usd / reward_usd * 100.0 } else { 0.0 },
                    cfg.mining.profit_ratio * 100.0
                );
                continue;
            }

            // fire-and-continue: 后台 spawn 发 tx, 主循环立即继续挖
            println!(
                "[发送] mine(0x{:016x}) · tip={:.2} gwei · gas=${:.2} · 净=${:.2}",
                hit.nonce,
                tip_now,
                cost_usd,
                reward_usd - cost_usd
            );
            let chain_for_submit = chain_ctx.clone();
            let challenge_for_submit = challenge_now;
            let mined = total_mined.clone();
            let failed = total_failed.clone();
            let nonce_for_log = hit.nonce;
            let printed_for_submit = printed_once.clone();
            tokio::spawn(async move {
                // 帮手: 打印事件前先固化当前进度行, 避免被覆盖
                let detach_progress = || {
                    if printed_for_submit.swap(false, Ordering::Relaxed) {
                        eprintln!();
                    }
                };
                match chain_for_submit
                    .submit_mine(nonce_for_log, challenge_for_submit, tip_now)
                    .await
                {
                    Ok(SubmitOutcome::Confirmed { tx_hash, block, gas_used }) => {
                        let n = mined.fetch_add(1, Ordering::Relaxed) + 1;
                        detach_progress();
                        println!(
                            "[确认] nonce=0x{:016x} · 区块 #{} · gas {} · tx 0x{} · ✓ 累计 {} 次",
                            nonce_for_log,
                            block,
                            gas_used,
                            hex::encode(tx_hash.as_slice()),
                            n,
                        );
                    }
                    Ok(SubmitOutcome::Reverted { tx_hash, reason }) => {
                        failed.fetch_add(1, Ordering::Relaxed);
                        detach_progress();
                        eprintln!(
                            "[失败] tx 0x{} revert: {}",
                            hex::encode(tx_hash.as_slice()),
                            reason
                        );
                    }
                    Ok(SubmitOutcome::TimeoutChallengeChanged { tx_hash }) => {
                        mined.fetch_add(1, Ordering::Relaxed);
                        detach_progress();
                        println!(
                            "[超时] tx 0x{} 超时但 challenge 已变, 视为成功",
                            hex::encode(tx_hash.as_slice())
                        );
                    }
                    Ok(SubmitOutcome::TimeoutDropped { tx_hash }) => {
                        failed.fetch_add(1, Ordering::Relaxed);
                        detach_progress();
                        eprintln!(
                            "[丢弃] tx 0x{} 超时且 challenge 未变, 被丢弃 · 重同步 nonce 池",
                            hex::encode(tx_hash.as_slice())
                        );
                        let _ = chain_for_submit.resync_nonce().await;
                    }
                    Err(e) => {
                        failed.fetch_add(1, Ordering::Relaxed);
                        detach_progress();
                        eprintln!(
                            "[错误] nonce=0x{:016x} 发送失败: {e:#} · 重同步 nonce 池",
                            nonce_for_log
                        );
                        let _ = chain_for_submit.resync_nonce().await;
                    }
                }
            });

            // 不等回执, 主循环继续等下个 hit
        }

        // 退出内层 (challenge 变化 / stop): 等所有 GPU 任务退出再继续外层
        watcher.abort();
        progress_task.abort();
        for t in card_tasks {
            let _ = t.await;
        }
        if stop.load(Ordering::SeqCst) {
            break 'outer;
        }
        if challenge_changed.load(Ordering::SeqCst) {
            detach(&printed_once);
            println!("[纪元] 检测到 challenge 变化, 重读 snapshot…");
        }
    }

    println!(
        "[结束] 累计成功 {} 次 · 失败/丢弃 {} 次",
        total_mined.load(Ordering::Relaxed),
        total_failed.load(Ordering::Relaxed)
    );
    Ok(())
}

async fn run_selftest(gpu: &GpuMiner) -> Result<()> {
    println!("[自检] 模式: 跑一个低难度 batch 校验 shader");
    // 用一个固定 challenge, 难度设成"前 8 bit = 0" (1/256), 16M nonce 内必命中
    let challenge = [0x11u8; 32];
    let mut difficulty = [0xffu8; 32];
    difficulty[0] = 0x01; // 任何 hash 第一字节 < 0x01 即 = 0

    println!("[自检] challenge = 0x{}", hex::encode(challenge));
    println!("[自检] difficulty = 0x{}", hex::encode(difficulty));
    println!("[自检] 起始 nonce = 0, 期望首字节为 0 的 hash...");

    let stop = std::sync::atomic::AtomicBool::new(false);
    // 自检用固定 batch=4M / depth=1
    let hit = gpu
        .search(
            4 * 1024 * 1024,
            1,
            challenge,
            difficulty,
            0,
            || stop.load(Ordering::Relaxed),
            |_| {},
        )
        .await?;
    let hit = hit.ok_or_else(|| anyhow!("[自检] batch 跑完未命中, shader 可能有问题"))?;

    println!("[自检] GPU 命中 nonce = {}", hit.nonce);
    println!("[自检] GPU hash = 0x{}", hex::encode(hit.hash_be));

    // CPU 复算
    use tiny_keccak::{Hasher, Keccak};
    let mut input = [0u8; 64];
    input[0..32].copy_from_slice(&challenge);
    input[56..64].copy_from_slice(&hit.nonce.to_be_bytes());
    let mut k = Keccak::v256();
    k.update(&input);
    let mut out = [0u8; 32];
    k.finalize(&mut out);
    println!("[自检] CPU hash = 0x{}", hex::encode(out));

    if out != hit.hash_be {
        return Err(anyhow!("[自检] ❌ GPU/CPU 不一致, shader 有 bug"));
    }
    if out >= difficulty {
        return Err(anyhow!("[自检] ❌ hash 不满足难度"));
    }
    println!("[自检] ✓ GPU 与 CPU keccak 完全一致, shader 正确");
    Ok(())
}

fn verify_hit(
    challenge: &[u8; 32],
    nonce: u64,
    expected_hash_be: &[u8; 32],
    difficulty_be: &[u8; 32],
) -> Result<()> {
    use tiny_keccak::{Hasher, Keccak};
    // 合约 abi.encodePacked(bytes32, uint256) = 32 + 32 字节
    // shader 里 nonce 高 24 字节强制 0, lane 7 = nonce uint64 BE @ byte 56-63
    let mut input = [0u8; 64];
    input[0..32].copy_from_slice(challenge);
    input[56..64].copy_from_slice(&nonce.to_be_bytes());

    let mut k = Keccak::v256();
    k.update(&input);
    let mut out = [0u8; 32];
    k.finalize(&mut out);

    if &out != expected_hash_be {
        return Err(anyhow::anyhow!(
            "CPU hash {} ≠ GPU hash {}",
            hex::encode(out),
            hex::encode(expected_hash_be)
        ));
    }
    if out >= *difficulty_be {
        return Err(anyhow::anyhow!(
            "hash {} 不满足难度 {}",
            hex::encode(out),
            hex::encode(difficulty_be)
        ));
    }
    Ok(())
}

fn rand_seed() -> u64 {
    use std::time::SystemTime;
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // 混入一点 process id 防止多实例撞起点
    let pid = std::process::id() as u128;
    let mut s = (nanos ^ (pid * 0x9E3779B97F4A7C15)) as u64;
    s ^= s >> 33;
    s = s.wrapping_mul(0xff51afd7ed558ccd);
    s ^= s >> 33;
    s = s.wrapping_mul(0xc4ceb9fe1a85ec53);
    s ^ (s >> 33)
}
