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

use anyhow::{bail, Context, Result};
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
}

impl Fill {
    fn simulated() -> Self {
        Self {
            order_id: "DRY_RUN".to_string(),
            status: "simulated".to_string(),
            success: true,
            simulated: true,
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

    pub fn is_live(&self) -> bool {
        matches!(self, Self::Live { .. })
    }

    /// 买入指定 token：GTC 限价单，价格用当前盘口 ask（marketable，立即成交为主）。
    pub async fn buy(&self, token_id: &str, price: f64, shares: f64) -> Result<Fill> {
        match self {
            Self::DryRun => Ok(Fill::simulated()),
            Self::Live { client, signer } => {
                let tid = U256::from_str(token_id)
                    .with_context(|| format!("token_id 解析失败: {token_id}"))?;
                let p = SdkDecimal::from_str(&format!("{price:.3}"))
                    .context("价格转换失败")?;
                let s = SdkDecimal::from_str(&format!("{shares}"))
                    .context("份额转换失败")?;

                let resp = client
                    .limit_order()
                    .token_id(tid)
                    .side(Side::Buy)
                    .price(p)
                    .size(s)
                    .order_type(OrderType::GTC)
                    .build_sign_and_post(signer)
                    .await
                    .context("提交订单失败")?;

                if !resp.success {
                    warn!("[EXEC] 订单未成功: id={} status={} err={:?}",
                        resp.order_id, resp.status, resp.error_msg);
                }
                Ok(Fill {
                    order_id: resp.order_id,
                    status: resp.status.to_string(),
                    success: resp.success,
                    simulated: false,
                })
            }
        }
    }
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

/// LIVE 模式下用于启动期凭证自检（main 在进入循环前调用，失败即退出）。
pub fn require_live_ready(exec: &OrderExecutor, config: &Config) -> Result<()> {
    if !config.dry_run && !exec.is_live() {
        bail!("DRY_RUN=0 但执行器未进入实盘模式");
    }
    Ok(())
}
