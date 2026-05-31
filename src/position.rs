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

    // Down 仓位
    pub down_shares: f64,
    pub down_cost_total: f64,

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
        if trade.side == "Up" {
            self.up_shares += trade.shares;
            self.up_cost_total += trade.total_cost;
        } else {
            self.down_shares += trade.shares;
            self.down_cost_total += trade.total_cost;
        }
        self.trades.push(trade);
    }

    /// 如果 Up 赢，当前已有仓位的 PnL
    pub fn pnl_if_up_wins(&self) -> f64 {
        self.up_shares * (1.0 - self.up_avg_full())
            - self.down_cost_total
    }

    /// 如果 Down 赢，当前已有仓位的 PnL
    pub fn pnl_if_down_wins(&self) -> f64 {
        self.down_shares * (1.0 - self.down_avg_full())
            - self.up_cost_total
    }

    /// 当前最坏情形 PnL（无论谁赢都取较差那个）
    pub fn worst_pnl(&self) -> f64 {
        self.pnl_if_up_wins().min(self.pnl_if_down_wins())
    }

    /// 假设再买 `shares` 份 `side` 方向（价格 `price`），最坏情形 PnL 会变成多少
    pub fn worst_pnl_if_add(&self, side: &str, price: f64, shares: f64) -> f64 {
        let full_c = full_cost_per_share(price);
        let cost   = full_c * shares;
        let (us, uc, ds, dc) = if side == "Up" {
            (self.up_shares + shares, self.up_cost_total + cost,
             self.down_shares,        self.down_cost_total)
        } else {
            (self.up_shares,   self.up_cost_total,
             self.down_shares + shares, self.down_cost_total + cost)
        };
        let ua = if us > 0.0 { uc / us } else { 0.0 };
        let da = if ds > 0.0 { dc / ds } else { 0.0 };
        let pnl_up  = us * (1.0 - ua) - dc;
        let pnl_dn  = ds * (1.0 - da) - uc;
        pnl_up.min(pnl_dn)
    }
}
