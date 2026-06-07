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

use anyhow::{ensure, Context, Result};
use tracing::{info, warn};

use alloy::signers::Signer as _;
use alloy::signers::local::PrivateKeySigner;
use polymarket_client_sdk_v2::auth::state::Authenticated;
use polymarket_client_sdk_v2::auth::Normal;
use polymarket_client_sdk_v2::clob::types::{Amount, OrderType, Side, SignatureType};
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

fn normalize_order_shares(shares: f64) -> Result<f64> {
    ensure!(
        shares.is_finite() && shares > 0.0,
        "下单份额必须是正的有限数字: {shares}"
    );
    Ok(shares.round().max(1.0))
}

fn clob_price_string(price: f64) -> String {
    let mut s = format!("{price:.4}");
    while s.contains('.') && s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.push('0');
    }
    s
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
        // 预热全局 version 缓存(SDK 内 AtomicU32,重启后清零)。build 内部要 resolve_version,
        // 不预热则重启后首单 build 走一次 GET /version(实测暴雷 1203ms)。一次永久(直到重启)。
        let v0 = std::time::Instant::now();
        match client.version().await {
            Ok(v) => info!("[EXEC] version 预热 ok=v{v} in {}ms", v0.elapsed().as_millis()),
            Err(e) => warn!("[EXEC] version 预热失败(首单 build 会慢): {e}"),
        }
        Ok(Self::Live { client: Box::new(client), signer })
    }

    /// 买入指定 token。
    ///
    /// LIVE 用 **FOK（fill-or-kill）**：要么按请求份额全部立即成交，要么整单取消，
    /// 不接受部分成交——避免延迟到达时盘口已变、只吃到零头或买在坏价。
    /// 代价：浅流动性下整单失败(扑空)概率高于 FAK。
    /// 默认用 SDK market order + Amount::shares，按"目标份额"下单。
    /// limit_price=None 时由 SDK 读取订单簿并计算吃满目标份额所需的 cutoff 价格；
    /// limit_price=Some(x) 时作为价格上限。
    pub async fn buy(&self, token_id: &str, price: f64, shares: f64, limit_price: Option<f64>) -> Result<Fill> {
        self.buy_with(token_id, price, shares, limit_price, OrderType::FOK).await
    }

    /// FAK 下单(吃掉簿上≤limit的量、最多 shares 份,剩余撤):需要"能成多少成多少"
    /// 的累积建仓场景用,部分成交是好事(不浪费低价机会)。当前未被任何策略调用,保留备用。
    pub async fn buy_fak(&self, token_id: &str, price: f64, shares: f64, limit_price: Option<f64>) -> Result<Fill> {
        self.buy_with(token_id, price, shares, limit_price, OrderType::FAK).await
    }

    async fn buy_with(&self, token_id: &str, price: f64, shares: f64, limit_price: Option<f64>, order_type: OrderType) -> Result<Fill> {
        let order_shares = normalize_order_shares(shares)?;
        match self {
            Self::DryRun => Ok(Fill::simulated(price, order_shares)),
            Self::Live { client, signer } => {
                let tid = U256::from_str(token_id)
                    .with_context(|| format!("token_id 解析失败: {token_id}"))?;
                let s = SdkDecimal::from_str(&format!("{order_shares:.0}"))
                    .context("份额转换失败")?;
                let amount = Amount::shares(s).context("份额转换失败")?;
                let price_cap = limit_price
                    .map(|x| SdkDecimal::from_str(&clob_price_string(x.clamp(0.01, 0.99))))
                    .transpose()
                    .context("价格转换失败")?;

                // 拆分埋点:build(取tick_size,预热后应命中缓存) / sign(本地EIP712) / post(POST /order)
                // 各自计时,精确定位 ~400-1000ms 到底卡在哪一步。
                let tb = std::time::Instant::now();
                let mut order_builder = client
                    .market_order()
                    .token_id(tid)
                    .side(Side::Buy)
                    .amount(amount)
                    .order_type(order_type.clone());
                if let Some(p) = price_cap {
                    order_builder = order_builder.price(p);
                }
                let order = order_builder.build().await.context("build 失败")?;
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
                // FOK：要么全额成交(taking≈order_shares)，要么整单 kill(taking=0)。
                // FAK：最多 order_shares 份,部分成交(taking∈(0,shares))属正常。
                let filled = taking > 0.0;
                let full_fok = matches!(order_type, OrderType::FOK) && (taking - order_shares).abs() <= 0.01;
                let booked_shares = if full_fok { order_shares } else { taking };
                let (filled_price, filled_shares) = if filled {
                    (making / booked_shares, booked_shares)
                } else {
                    (price, 0.0)
                };

                if !filled {
                    warn!("[EXEC] 订单未成交(不记账): id={} status={} ok={} err={:?}",
                        resp.order_id, resp.status, resp.success, resp.error_msg);
                } else if matches!(order_type, OrderType::FOK) && !full_fok {
                    warn!("[EXEC] 部分成交: 请求{order_shares:.0}份 实际{taking:.6}份 @ {:.3}",
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
                // 串行(非并发):只占一条连接。并发会建多条连接,多余的空闲 5 分钟被 CF 切,
                // 下单随机命中冷条→暴雷(2161ms)。串行让连接池只剩一条,warm 与 post 必走同一条热连接。
                let s = std::time::Instant::now(); let _ = client.tick_size(tid).await; let ts = s.elapsed();
                let s = std::time::Instant::now(); let _ = client.neg_risk(tid).await; let nr = s.elapsed();
                let s = std::time::Instant::now(); let _ = client.fee_rate_bps(tid).await; let fee = s.elapsed();
                let n = token_id.len();
                tracing::info!(
                    "[CACHE_PREWARM] token=..{} tick_size={}ms neg_risk={}ms fee={}ms total={}ms",
                    &token_id[n.saturating_sub(8)..],
                    ts.as_millis(), nr.as_millis(), fee.as_millis(), t0.elapsed().as_millis()
                );
            }
        }
    }

    /// 连接保活:用**同一个 client**打**同一个 clob host**的 ok()(GET,SDK 不缓存),
    /// 让连接池保持一条热连接,使紧接着的 post_order 复用它、免 TCP+TLS 握手(~200ms)。
    /// reqwest+Cloudflare 约 90-100s 掐空闲连接,故必须**抢单前 <90s**调(启动只预热一次没用)。
    /// DryRun 下 no-op。
    pub async fn prewarm(&self) {
        if let Self::Live { client, .. } = self {
            let t = std::time::Instant::now();
            match client.ok().await {
                Ok(_) => tracing::info!("[ORDER_PREWARM] warm in {}ms", t.elapsed().as_millis()),
                Err(e) => tracing::warn!("[ORDER_PREWARM] failed: {e}"),
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
        let order_shares = normalize_order_shares(shares)?;
        match self {
            Self::DryRun => Ok(Fill::simulated(price, order_shares)),
            Self::Live { client, signer } => {
                let tid = U256::from_str(token_id)
                    .with_context(|| format!("token_id 解析失败: {token_id}"))?;
                // maker 不加缓冲,就挂在该价位等成交。
                let limit = price.clamp(0.01, 0.99);
                let p = SdkDecimal::from_str(&clob_price_string(limit))
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
                let full_fill = (taking - order_shares).abs() <= 0.01;
                let booked_shares = if full_fill { order_shares } else { taking };
                let (filled_price, filled_shares) = if taking > 0.0 {
                    (making / booked_shares, booked_shares)
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

    /// 列出账户当前所有挂单（对账用）。
    ///
    /// 走 `client.orders()`(`data/orders` 列表)翻页(最多 5 页)。语义同 `maker_fills`:
    /// 只返回**未全成**的单——全成交后从列表消失。DryRun 返回空。
    pub async fn list_open_orders(&self) -> Result<Vec<OpenOrderInfo>> {
        use polymarket_client_sdk_v2::clob::types::request::OrdersRequest;
        match self {
            Self::DryRun => Ok(vec![]),
            Self::Live { client, .. } => {
                let req = OrdersRequest::builder().build();
                let mut out: Vec<OpenOrderInfo> = Vec::new();
                let mut cursor: Option<String> = None;
                let mut pages = 0u32;
                loop {
                    let page = client.orders(&req, cursor.clone()).await
                        .context("查询挂单失败")?;
                    for o in &page.data {
                        out.push(OpenOrderInfo {
                            order_id: o.id.clone(),
                            token_id: o.asset_id.to_string(),
                            side: o.side.to_string(),
                            price: o.price.to_string().parse::<f64>().unwrap_or(0.0),
                            original_size: o.original_size.to_string().parse::<f64>().unwrap_or(0.0),
                            size_matched: o.size_matched.to_string().parse::<f64>().unwrap_or(0.0),
                        });
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

    /// 查 USDC 余额（对账用）。返回人类单位美元（CLOB balance-allowance 的 balance 字段
    /// 已是美元单位，非 6 位 base unit，无需除 1e6）。DryRun 返回 0.0。
    pub async fn usdc_balance(&self) -> Result<f64> {
        use polymarket_client_sdk_v2::clob::types::request::BalanceAllowanceRequest;
        use polymarket_client_sdk_v2::clob::types::AssetType;
        match self {
            Self::DryRun => Ok(0.0),
            Self::Live { client, .. } => {
                let req = BalanceAllowanceRequest::builder()
                    .asset_type(AssetType::Collateral)
                    .build();
                let r = client.balance_allowance(req).await.context("查询余额失败")?;
                Ok(r.balance.to_string().parse::<f64>().unwrap_or(0.0))
            }
        }
    }

    /// 查某 token 最优买一/卖一，返回 `(best_bid, best_ask)`。
    /// asks 最低价=ask，bids 最高价=bid。空簿对应侧返回 0.0。
    /// DryRun 返回 (0.0, 0.0)。
    pub async fn best_bid_ask(&self, token_id: &str) -> Result<(f64, f64)> {
        use polymarket_client_sdk_v2::clob::types::request::OrderBookSummaryRequest;
        match self {
            Self::DryRun => Ok((0.0, 0.0)),
            Self::Live { client, .. } => {
                let tid = U256::from_str(token_id)
                    .with_context(|| format!("token_id 解析失败: {token_id}"))?;
                let req = OrderBookSummaryRequest::builder().token_id(tid).build();
                let book = client.order_book(&req).await.context("查询盘口失败")?;
                // bid = bids 最高价；ask = asks 最低价。空簿返回 0.0。
                let best_bid = book.bids.iter()
                    .filter_map(|l| l.price.to_string().parse::<f64>().ok())
                    .fold(0.0_f64, f64::max);
                let best_ask = book.asks.iter()
                    .filter_map(|l| l.price.to_string().parse::<f64>().ok())
                    .fold(f64::INFINITY, f64::min);
                let best_ask = if best_ask.is_finite() { best_ask } else { 0.0 };
                Ok((best_bid, best_ask))
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

/// 账户一张挂单的对账信息（list_open_orders 返回）。
#[derive(Debug, Clone)]
pub struct OpenOrderInfo {
    pub order_id: String,
    pub token_id: String,
    /// "BUY" / "SELL"（SDK Side 枚举的大写字符串）。
    pub side: String,
    pub price: f64,
    pub original_size: f64,
    pub size_matched: f64,
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

#[cfg(test)]
mod tests {
    use super::{clob_price_string, normalize_order_shares};

    #[test]
    fn order_shares_are_whole_numbers() {
        assert_eq!(normalize_order_shares(20.49).unwrap(), 20.0);
        assert_eq!(normalize_order_shares(20.50).unwrap(), 21.0);
        assert_eq!(normalize_order_shares(0.40).unwrap(), 1.0);
    }

    #[test]
    fn order_shares_reject_non_finite_values() {
        assert!(normalize_order_shares(f64::NAN).is_err());
        assert!(normalize_order_shares(f64::INFINITY).is_err());
    }

    #[test]
    fn order_shares_reject_non_positive_values() {
        assert!(normalize_order_shares(0.0).is_err());
        assert!(normalize_order_shares(-3.0).is_err());
    }

    #[test]
    fn clob_price_string_preserves_tick_precision() {
        assert_eq!(clob_price_string(0.537), "0.537");
        assert_eq!(clob_price_string(0.52), "0.52");
        assert_eq!(clob_price_string(0.5), "0.5");
        assert_eq!(clob_price_string(0.1234), "0.1234");
    }
}
