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

    /// 当前加入新仓后，是否仍然可以锁利润
    /// new_side: 要追的方向
    /// new_shares: 追的份数
    /// new_price: 追的价格
    /// opp_ask: 对立方向当前盘口价格
    pub fn can_chase_then_lock(
        &self,
        new_side: &str,
        new_shares: f64,
        new_price: f64,
        opp_ask: f64,
        target_profit_per_share: f64,
    ) -> bool {
        let new_full = full_cost_per_share(new_price);
        let opp_full = full_cost_per_share(opp_ask);

        let new_avg = if new_side == "Up" {
            (self.up_cost_total + new_shares * new_full) / (self.up_shares + new_shares)
        } else {
            (self.down_cost_total + new_shares * new_full) / (self.down_shares + new_shares)
        };

        new_avg + opp_full < 1.0 - target_profit_per_share
    }

    /// 是否可以锁利润（买对立方向）
    /// opp_ask: 对立方向当前价
    /// target: 期望锁定的利润/份（如 0.02）
    pub fn can_lock_profit(&self, opp_ask: f64, target: f64) -> bool {
        let a = if self.up_shares > 0.0 { self.up_avg_full() }
                else { self.down_avg_full() };
        let d = full_cost_per_share(opp_ask);
        a + d < 1.0 - target
    }

    /// 锁亏损：买多少份对立方向，把 Down 赢时的亏损控制在 max_loss
    /// 公式：q_D = (q_U × a* - L) / (1 - d*)
    pub fn loss_lock_shares(&self, opp_ask: f64, max_loss: f64) -> f64 {
        let (q_main, a) = if self.up_shares > 0.0 {
            (self.up_shares, self.up_avg_full())
        } else {
            (self.down_shares, self.down_avg_full())
        };
        let d = full_cost_per_share(opp_ask);
        let numerator = q_main * a - max_loss;
        if numerator <= 0.0 || d >= 1.0 { return 0.0; }
        (numerator / (1.0 - d)).max(0.0)
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

    /// 预计锁定损益（如果现在以 opp_ask 买对立方向同等份数）
    pub fn projected_locked_pnl(&self, opp_ask: f64) -> f64 {
        let (q, a) = if self.up_shares > 0.0 {
            (self.up_shares, self.up_avg_full())
        } else {
            (self.down_shares, self.down_avg_full())
        };
        let d = full_cost_per_share(opp_ask);
        q * (1.0 - a - d)
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

    /// 总已投入成本
    pub fn total_invested(&self) -> f64 {
        self.up_cost_total + self.down_cost_total
    }
}
