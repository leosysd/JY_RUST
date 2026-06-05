use serde::{Deserialize, Serialize};

/// Polymarket crypto taker 手续费：fee = price × (1-price) × 0.07
pub fn taker_fee(price: f64) -> f64 {
    0.07 * price * (1.0 - price)
}

/// 含手续费的全成本/份
pub fn full_cost_per_share(price: f64) -> f64 {
    price + taker_fee(price)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeRecord {
    pub side: String,        // "Up" / "Down"
    pub shares: f64,
    pub price: f64,
    pub fee_per_share: f64,
    pub full_cost_per_share: f64,
    pub total_cost: f64,
    pub phase: String,       // "entry" / "chase" / "lock_profit" / "lock_loss"
    pub ts: i64,
    pub time_bj: String,
}

/// 一张挂在簿上的 maker 订单（路线二）。持久化于 state，跨 tick 追踪成交进度。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenOrder {
    pub order_id: String,
    pub side: String,        // "Up" / "Down"
    pub price: f64,
    pub size: f64,
    /// 已记账的成交份额；每 tick 查 size_matched，增量部分补记成交。
    pub matched_recorded: f64,
    pub placed_ts: i64,
    pub phase: String,       // 触发该挂单的策略阶段标签（如 "scalein"）
    /// 是否曾在 orders() 列表里确认挂上过。用于区分"全成交消失"与"刚挂未索引"。
    /// serde default：兼容旧 state 文件（无此字段时为 false）。
    #[serde(default)]
    pub seen_live: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub enum Phase {
    #[default]
    Waiting,   // 等待入场
    Holding,   // 有仓位，监控追仓/锁仓
    Locked,    // 两边都买了，等结算
    Settled,   // 结算完毕
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MarketPosition {
    pub slug: String,
    pub end_ts: i64,
    pub price_to_beat: f64,   // B = 开盘 Chainlink 价

    // Up 仓位
    pub up_shares: f64,
    pub up_cost_total: f64,   // 含手续费总成本
    /// Up 本金(price×shares,不含费)。用户口径:输方归零只损本金、不扣手续费,
    /// 故"对侧赢"时用本金、不用含费成本。serde default 兼容旧 state。
    #[serde(default)]
    pub up_principal: f64,

    // Down 仓位
    pub down_shares: f64,
    pub down_cost_total: f64,
    #[serde(default)]
    pub down_principal: f64,

    pub trades: Vec<TradeRecord>,
    pub phase: Phase,

    /// 路线二：当前挂在簿上未结的 maker 单。旧 state 文件无此字段时默认空。
    #[serde(default)]
    pub open_orders: Vec<OpenOrder>,

    // 结算
    pub winner: Option<String>,
    pub realized_pnl: Option<f64>,
}

impl MarketPosition {
    pub fn new(slug: &str, end_ts: i64, price_to_beat: f64) -> Self {
        Self {
            slug: slug.to_string(),
            end_ts,
            price_to_beat,
            ..Default::default()
        }
    }

    /// Up 平均全成本/份 (a*)
    pub fn up_avg_full(&self) -> f64 {
        if self.up_shares <= 0.0 { return 0.0; }
        self.up_cost_total / self.up_shares
    }

    /// Down 平均全成本/份 (d*)
    pub fn down_avg_full(&self) -> f64 {
        if self.down_shares <= 0.0 { return 0.0; }
        self.down_cost_total / self.down_shares
    }

    /// 添加一笔交易
    pub fn add_trade(&mut self, trade: TradeRecord) {
        let principal = trade.price * trade.shares;   // 本金(不含费)
        if trade.side == "Up" {
            self.up_shares += trade.shares;
            self.up_cost_total += trade.total_cost;
            self.up_principal += principal;
        } else {
            self.down_shares += trade.shares;
            self.down_cost_total += trade.total_cost;
            self.down_principal += principal;
        }
        self.trades.push(trade);
    }

    /// 如果 Up 赢：Up 兑付得含费收益(up_shares − up_cost_total) − Down 输了的本金。
    /// 用户口径:Down 归零只损本金(down_principal),不扣 Down 的手续费。
    pub fn pnl_if_up_wins(&self) -> f64 {
        (self.up_shares - self.up_cost_total) - self.down_principal
    }

    /// 如果 Down 赢：对称——Down 含费收益 − Up 本金。
    pub fn pnl_if_down_wins(&self) -> f64 {
        (self.down_shares - self.down_cost_total) - self.up_principal
    }

    /// 当前最坏情形 PnL（无论谁赢都取较差那个）
    pub fn worst_pnl(&self) -> f64 {
        self.pnl_if_up_wins().min(self.pnl_if_down_wins())
    }

    /// 假设再买 `shares` 份 `side` 方向（价格 `price`），最坏情形 PnL 会变成多少。
    /// 用户口径:赢方含费、输方只本金。
    pub fn worst_pnl_if_add(&self, side: &str, price: f64, shares: f64) -> f64 {
        let cost      = full_cost_per_share(price) * shares;  // 含费
        let principal = price * shares;                       // 本金
        let (us, uc, upr, ds, dc, dpr) = if side == "Up" {
            (self.up_shares + shares, self.up_cost_total + cost, self.up_principal + principal,
             self.down_shares, self.down_cost_total, self.down_principal)
        } else {
            (self.up_shares, self.up_cost_total, self.up_principal,
             self.down_shares + shares, self.down_cost_total + cost, self.down_principal + principal)
        };
        let pnl_up = (us - uc) - dpr;   // Up赢:Up含费收益 − Down本金
        let pnl_dn = (ds - dc) - upr;   // Down赢:Down含费收益 − Up本金
        pnl_up.min(pnl_dn)
    }
}
