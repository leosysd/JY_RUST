//! 统一下单执行器。
//!
//! 两种模式由 DRY_RUN 决定：
//!   - DRY_RUN=1：模拟，不发真实订单，立即返回模拟成交。
//!   - DRY_RUN=0：通过官方 Polymarket CLOB V2 SDK 真实下单（签名、合约地址、
//!     V2 域分隔符全部由官方维护，避免手搓签名出错）。
//!
//! 启动时（LIVE）会用 PRIVATE_KEY 调 `.authenticate()` 自动派生/校验 API creds，
//! 派生失败会直接 bail，等于在启动阶段就"确认 API creds 可用"。

use std::str::FromStr;

use anyhow::{Context, Result};
use tracing::{info, warn};

use alloy::signers::Signer as _;
use alloy::signers::local::PrivateKeySigner;
use polymarket_client_sdk_v2::auth::state::Authenticated;
use polymarket_client_sdk_v2::auth::Normal;
use polymarket_client_sdk_v2::clob::types::{OrderType, Side, SignatureType};
use polymarket_client_sdk_v2::clob::{Client, Config as ClobConfig};
use polymarket_client_sdk_v2::types::{Address, Decimal as SdkDecimal, U256};
use polymarket_client_sdk_v2::POLYGON;

use crate::config::Config;

/// 一次下单的结果。
#[derive(Debug, Clone)]
pub struct Fill {
    pub order_id: String,
    pub status: String,
    pub success: bool,
    pub simulated: bool,
    /// 真实成交均价（实盘从 makingAmount/takingAmount 反推；模拟=请求价）
    pub filled_price: f64,
    /// 真实成交份额（实盘=takingAmount；模拟=请求份额）
    pub filled_shares: f64,
}

impl Fill {
    fn simulated(price: f64, shares: f64) -> Self {
        Self {
            order_id: "DRY_RUN".to_string(),
            status: "simulated".to_string(),
            success: true,
            simulated: true,
            filled_price: price,
            filled_shares: shares,
        }
    }
}

pub enum OrderExecutor {
    /// 模拟：不发真实订单
    DryRun,
    /// 实盘：持有已认证的官方 V2 客户端与签名器
    Live {
        client: Box<Client<Authenticated<Normal>>>,
        signer: PrivateKeySigner,
    },
}

impl OrderExecutor {
    /// 根据配置构建执行器。LIVE 模式会在此完成认证（派生 API creds）。
    pub async fn new(config: &Config) -> Result<Self> {
        if config.dry_run {
            info!("[EXEC] DRY_RUN=1 模拟模式，不会发出真实订单");
            return Ok(Self::DryRun);
        }

        let pk = config.private_key.as_ref()
            .context("DRY_RUN=0 需要 PRIVATE_KEY")?;
        let signer = PrivateKeySigner::from_str(pk.trim())
            .context("PRIVATE_KEY 解析失败")?
            .with_chain_id(Some(POLYGON));

        let builder = Client::new(&config.clob_v2_api_url, ClobConfig::default())
            .context("创建 CLOB V2 客户端失败")?
            .authentication_builder(&signer);

        // 代理钱包(funder) 路径 vs EOA 直签路径
        let client = if let Some(dw) = &config.deposit_wallet {
            let funder = Address::from_str(dw.trim())
                .context("DEPOSIT_WALLET_ADDRESS 解析失败")?;
            let sig_type = map_sig_type(config.signature_type);
            info!("[EXEC] LIVE 认证中：funder={dw} sig_type={sig_type:?} ...");
            builder
                .funder(funder)
                .signature_type(sig_type)
                .authenticate()
                .await
                .context("认证/派生 API creds 失败（检查 PRIVATE_KEY/DEPOSIT_WALLET/网络）")?
        } else {
            info!("[EXEC] LIVE 认证中：EOA 直签 ...");
            builder
                .authenticate()
                .await
                .context("认证/派生 API creds 失败")?
        };

        info!("[EXEC] DRY_RUN=0 实盘模式就绪，API creds 已派生确认");
        Ok(Self::Live { client: Box::new(client), signer })
    }

    /// 买入指定 token。
    ///
    /// LIVE 用 **FAK（fill-and-kill）**：立即吃单，能成交多少成交多少，剩余取消。
    /// 比 FOK 更适合 5 分钟盘口的浅流动性——部分成交也按真实成交份额记账。
    /// 限价加 `MARKETABLE_BUFFER` 让单子能穿透盘口若干档。
    pub async fn buy(&self, token_id: &str, price: f64, shares: f64) -> Result<Fill> {
        match self {
            Self::DryRun => Ok(Fill::simulated(price, shares)),
            Self::Live { client, signer } => {
                let tid = U256::from_str(token_id)
                    .with_context(|| format!("token_id 解析失败: {token_id}"))?;
                // marketable 限价：在 ask 上加缓冲并对齐到 0.01 tick，封顶 0.99。
                // 缓冲(滑点容忍)可经 env TAKER_BUFFER 调整，默认 MARKETABLE_BUFFER(0.02)。
                // 只在首单读一次 env 并缓存,后续下单零 getenv(见 taker_buffer)。
                let buffer = taker_buffer();
                let limit = ((price + buffer).min(0.99) * 100.0).round() / 100.0;
                // 份额取整：Polymarket 要求 BUY 金额(price×shares)≤2位小数。
                // 价格已是2位小数，份额取整 → 乘积必然≤2位小数，金额合法。
                // 成交返回的零头(如5.158729)若直接下单会算出4位小数金额被拒。
                let order_shares = shares.round().max(1.0);
                let p = SdkDecimal::from_str(&format!("{limit:.2}"))
                    .context("价格转换失败")?;
                let s = SdkDecimal::from_str(&format!("{order_shares:.0}"))
                    .context("份额转换失败")?;

                // 拆分埋点:build(取tick_size,预热后应命中缓存) / sign(本地EIP712) / post(POST /order)
                // 各自计时,精确定位 ~400-1000ms 到底卡在哪一步。
                let tb = std::time::Instant::now();
                let order = client
                    .limit_order()
                    .token_id(tid)
                    .side(Side::Buy)
                    .price(p)
                    .size(s)
                    .order_type(OrderType::FAK)
                    .build()
                    .await
                    .context("build 失败")?;
                let t_build = tb.elapsed();
                let tsg = std::time::Instant::now();
                let signed = client.sign(signer, order).await.context("sign 失败")?;
                let t_sign = tsg.elapsed();
                let tp = std::time::Instant::now();
                let resp_result = client.post_order(signed).await;
                let t_post = tp.elapsed();
                tracing::info!(
                    "[ORDER_LAT] build={}ms sign={}ms post={}ms 总={}ms",
                    t_build.as_millis(), t_sign.as_millis(), t_post.as_millis(),
                    (t_build + t_sign + t_post).as_millis()
                );
                let resp = resp_result.context("提交订单失败")?;

                // 从真实成交额反推成交均价/份额：BUY → making=USDC支出, taking=买到份额
                let making = resp.making_amount.to_string().parse::<f64>().unwrap_or(0.0);
                let taking = resp.taking_amount.to_string().parse::<f64>().unwrap_or(0.0);
                // FAK 部分成交：以真实成交份额为准（taking>0 即视为成交）
                let filled = taking > 0.0;
                let (filled_price, filled_shares) = if filled {
                    (making / taking, taking)
                } else {
                    (price, 0.0)
                };

                if !filled {
                    warn!("[EXEC] 订单未成交(不记账): id={} status={} ok={} err={:?}",
                        resp.order_id, resp.status, resp.success, resp.error_msg);
                } else if (taking - shares).abs() > 0.01 {
                    warn!("[EXEC] 部分成交: 请求{shares:.0}份 实际{taking:.1}份 @ {:.3}",
                        making / taking);
                }
                Ok(Fill {
                    order_id: resp.order_id,
                    status: resp.status.to_string(),
                    success: filled,
                    simulated: false,
                    filled_price,
                    filled_shares,
                })
            }
        }
    }

    /// 预热 token 的 tick-size/neg-risk/fee 缓存(SDK 内部 DashMap)。
    /// 狙击在盘一出现时调:突破下单时 build_sign_and_post 命中缓存,省掉
    /// markets-by-token / clob-markets / tick-size 三个串行 API,
    /// 下单往返从 ~400-900ms 降到 ~50-100ms(只剩 POST /order)。DryRun 下 no-op。
    pub async fn prime_token(&self, token_id: &str) {
        if let Self::Live { client, .. } = self {
            if let Ok(tid) = U256::from_str(token_id) {
                // 并发取 3 个元数据填缓存,各自计时,忽略错误(预热失败只是下单回退到现取)。
                let t0 = std::time::Instant::now();
                let (ts, nr, fee) = tokio::join!(
                    async { let s = std::time::Instant::now(); let _ = client.tick_size(tid).await; s.elapsed() },
                    async { let s = std::time::Instant::now(); let _ = client.neg_risk(tid).await; s.elapsed() },
                    async { let s = std::time::Instant::now(); let _ = client.fee_rate_bps(tid).await; s.elapsed() },
                );
                let n = token_id.len();
                tracing::info!(
                    "[ORDER_PREWARM] token=..{} tick_size={}ms neg_risk={}ms fee={}ms total={}ms",
                    &token_id[n.saturating_sub(8)..],
                    ts.as_millis(), nr.as_millis(), fee.as_millis(), t0.elapsed().as_millis()
                );
            }
        }
    }

    // ── 路线二 maker 能力层（GTC + post_only，零 taker 费）──────────────────
    //
    // 设计见 reference_maker_state_machine。挂单后状态由 query_order 轮询，
    // 超时由 cancel 撤单。DryRun 下 place_maker 仍立即全额成交（沿用理想账语义），
    // 因此 DryRun 不会进入“挂单等收割”分支，maker 成交率必须 LIVE 小额实测。

    /// 挂 maker 限价单（GTC + post_only）。
    /// post_only 保证只做 maker：若该价会立即吃单，交易所直接拒绝（success=false），
    /// 从而永不付 taker 费。返回 Fill：
    ///   - DryRun：立即全额成交（filled_shares=shares）。
    ///   - LIVE 挂单成功：success=true、filled_shares=0（挂在簿上，等 query_order 收割）。
    ///   - LIVE 被拒（会吃单/越界）：success=false。
    pub async fn place_maker(&self, token_id: &str, price: f64, shares: f64) -> Result<Fill> {
        match self {
            Self::DryRun => Ok(Fill::simulated(price, shares)),
            Self::Live { client, signer } => {
                let tid = U256::from_str(token_id)
                    .with_context(|| format!("token_id 解析失败: {token_id}"))?;
                // maker 价对齐 0.01 tick；不加缓冲，就挂在该价位等成交。
                let limit = ((price.clamp(0.01, 0.99)) * 100.0).round() / 100.0;
                let order_shares = shares.round().max(1.0);
                let p = SdkDecimal::from_str(&format!("{limit:.2}"))
                    .context("价格转换失败")?;
                let s = SdkDecimal::from_str(&format!("{order_shares:.0}"))
                    .context("份额转换失败")?;

                let resp = client
                    .limit_order()
                    .token_id(tid)
                    .side(Side::Buy)
                    .price(p)
                    .size(s)
                    .order_type(OrderType::GTC)
                    .post_only(true)
                    .build_sign_and_post(signer)
                    .await
                    .context("提交 maker 订单失败")?;

                let making = resp.making_amount.to_string().parse::<f64>().unwrap_or(0.0);
                let taking = resp.taking_amount.to_string().parse::<f64>().unwrap_or(0.0);
                let (filled_price, filled_shares) = if taking > 0.0 {
                    (making / taking, taking)
                } else {
                    (limit, 0.0)
                };
                if !resp.success {
                    warn!("[EXEC MAKER] 挂单被拒: id={} status={} err={:?}",
                        resp.order_id, resp.status, resp.error_msg);
                }
                Ok(Fill {
                    order_id: resp.order_id,
                    status: resp.status.to_string(),
                    success: resp.success,
                    simulated: false,
                    filled_price,
                    filled_shares,
                })
            }
        }
    }

    /// 查一张挂单的成交进度。DryRun 视为已成交（占位，DryRun 不走轮询路径）。
    pub async fn query_order(&self, order_id: &str) -> Result<OrderState> {
        match self {
            Self::DryRun => Ok(OrderState {
                status: "matched".into(), size: 0.0, size_matched: 0.0, price: 0.0,
            }),
            Self::Live { client, .. } => {
                let o = client.order(order_id).await.context("查询订单失败")?;
                Ok(OrderState {
                    status: format!("{:?}", o.status).to_lowercase(),
                    size: o.original_size.to_string().parse().unwrap_or(0.0),
                    size_matched: o.size_matched.to_string().parse().unwrap_or(0.0),
                    price: o.price.to_string().parse().unwrap_or(0.0),
                })
            }
        }
    }

    /// 拉本账户**当前仍挂在簿上**的单，聚合成 `order_id -> (已成交份额, 挂单价)`。
    ///
    /// Bug2 修复历程：①旧 `query_order`(`data/order/{id}`) 单查刚挂/已成的单会 404；
    /// ②`data/trades` 又因官方 SDK 严格解析遇空字符串崩(invalid Decimal "")。
    /// 最终改走 `client.orders()`(`data/orders` 列表，返回 OpenOrderResponse 能正常解析、不 404)。
    /// 注意语义：orders 只返回**未全成**的单——单子全部成交后会从列表消失。
    /// 故调用方(harvest)需配合 `seen_live` 标记：曾出现在本表、之后消失 = 全成交补记。
    /// 翻页最多 5 页（5 分钟盘挂单数很少）。
    pub async fn maker_fills(&self) -> Result<std::collections::HashMap<String, (f64, f64)>> {
        use polymarket_client_sdk_v2::clob::types::request::OrdersRequest;
        let mut out: std::collections::HashMap<String, (f64, f64)> = std::collections::HashMap::new();
        match self {
            Self::DryRun => Ok(out),
            Self::Live { client, .. } => {
                let req = OrdersRequest::builder().build();
                let mut cursor: Option<String> = None;
                let mut pages = 0u32;
                loop {
                    let page = client.orders(&req, cursor.clone()).await
                        .context("查询挂单失败")?;
                    for o in &page.data {
                        let matched = o.size_matched.to_string().parse::<f64>().unwrap_or(0.0);
                        let price = o.price.to_string().parse::<f64>().unwrap_or(0.0);
                        out.insert(o.id.clone(), (matched, price));
                    }
                    pages += 1;
                    if page.next_cursor.is_empty() || page.data.is_empty() || pages >= 5 {
                        break;
                    }
                    cursor = Some(page.next_cursor.clone());
                }
                Ok(out)
            }
        }
    }

    /// 撤单。返回是否确实撤掉（已成交的单撤不掉，返回 false，靠下次 query 兜底收割）。
    pub async fn cancel(&self, order_id: &str) -> Result<bool> {
        match self {
            Self::DryRun => Ok(true),
            Self::Live { client, .. } => {
                let r = client.cancel_order(order_id).await.context("撤单失败")?;
                Ok(r.canceled.iter().any(|id| id == order_id))
            }
        }
    }

    /// 查询某 token 的真实手续费率（bps）。用于核对代码内写死的 7% 假费与实际值。
    pub async fn fee_rate_bps(&self, token_id: &str) -> Result<u32> {
        match self {
            Self::DryRun => Ok(0),
            Self::Live { client, .. } => {
                let tid = U256::from_str(token_id)
                    .with_context(|| format!("token_id 解析失败: {token_id}"))?;
                let r = client.fee_rate_bps(tid).await.context("查询费率失败")?;
                Ok(r.base_fee)
            }
        }
    }
}

/// 一张 maker 挂单的当前状态（query_order 返回）。
#[derive(Debug, Clone)]
pub struct OrderState {
    /// live / matched / canceled / unmatched / delayed / unknown
    pub status: String,
    /// 原始下单份额
    pub size: f64,
    /// 已成交份额（轮询此值的增量即可知新成交）
    pub size_matched: f64,
    /// 挂单价
    pub price: f64,
}

/// FOK 限价相对 ask 的上浮，使其可穿透盘口若干档成交。
const MARKETABLE_BUFFER: f64 = 0.02;

/// 下单滑点缓冲(TAKER_BUFFER)。只在首次调用时读一次环境变量并缓存,
/// 避免每次下单都走 getenv+parse(下单是延迟敏感路径)。
fn taker_buffer() -> f64 {
    use std::sync::OnceLock;
    static BUF: OnceLock<f64> = OnceLock::new();
    *BUF.get_or_init(|| {
        std::env::var("TAKER_BUFFER").ok()
            .and_then(|s| s.trim().parse::<f64>().ok())
            .filter(|b| *b >= 0.0 && *b <= 0.5)
            .unwrap_or(MARKETABLE_BUFFER)
    })
}

/// 配置中的 signature_type(u8) → SDK 枚举。
/// 0=EOA, 1=Proxy(email/magic), 2=GnosisSafe, 3=Poly1271(V2 智能合约钱包)
fn map_sig_type(t: u8) -> SignatureType {
    match t {
        0 => SignatureType::Eoa,
        1 => SignatureType::Proxy,
        2 => SignatureType::GnosisSafe,
        _ => SignatureType::Poly1271,
    }
}
