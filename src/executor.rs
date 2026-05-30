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
                // marketable 限价：在 ask 上加缓冲并对齐到 0.01 tick，封顶 0.99
                let limit = ((price + MARKETABLE_BUFFER).min(0.99) * 100.0).round() / 100.0;
                // 份额取整：Polymarket 要求 BUY 金额(price×shares)≤2位小数。
                // 价格已是2位小数，份额取整 → 乘积必然≤2位小数，金额合法。
                // 成交返回的零头(如5.158729)若直接下单会算出4位小数金额被拒。
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
                    .order_type(OrderType::FAK)
                    .build_sign_and_post(signer)
                    .await
                    .context("提交订单失败")?;

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
}

/// FOK 限价相对 ask 的上浮，使其可穿透盘口若干档成交。
const MARKETABLE_BUFFER: f64 = 0.02;

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
