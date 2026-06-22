#!/usr/bin/env python3
"""Replay accum from z_tick snapshots as a taker/FAK strategy simulator.

This is a market-data replay, not an intervention over existing trades. It
uses the recorded z_tick snapshots (Up/Down asks, z, p_up, seconds_left) and
settlement winners to run the accum decision logic with configurable rescue and
trend controls.
"""

from __future__ import annotations

import argparse
import collections
import itertools
import json
import math
import statistics
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import analyze_backtest as base


@dataclass
class Trade:
    side: str
    shares: float
    price: float
    phase: str
    ts: float


@dataclass
class Pos:
    up_shares: float = 0.0
    up_cost: float = 0.0
    up_principal: float = 0.0
    down_shares: float = 0.0
    down_cost: float = 0.0
    down_principal: float = 0.0
    trades: list[Trade] = field(default_factory=list)

    def add(
        self,
        side: str,
        shares: float,
        price: float,
        phase: str,
        ts: float,
        min_order_usdc: float = 0.0,
    ) -> None:
        if shares < 1.0:
            return
        shares = normal_order_shares(shares, price, min_order_usdc)
        cost = base.full_cost_per_share(price) * shares
        principal = price * shares
        if side == "Up":
            self.up_shares += shares
            self.up_cost += cost
            self.up_principal += principal
        else:
            self.down_shares += shares
            self.down_cost += cost
            self.down_principal += principal
        self.trades.append(Trade(side, shares, price, phase, ts))

    def pnl_decision(self, winner: str) -> float:
        if winner == "Up":
            return (self.up_shares - self.up_cost) - self.down_principal
        return (self.down_shares - self.down_cost) - self.up_principal

    def pnl_settle(self, winner: str) -> float:
        if winner == "Up":
            return (self.up_shares - self.up_cost) - self.down_cost
        return (self.down_shares - self.down_cost) - self.up_cost

    def worst_decision(self) -> float:
        return min(self.pnl_decision("Up"), self.pnl_decision("Down"))

    def worst_if_add(self, side: str, shares: float, price: float) -> float:
        q = math.floor(shares + 1e-9)
        cost = base.full_cost_per_share(price) * q
        principal = price * q
        us, uc, up = self.up_shares, self.up_cost, self.up_principal
        ds, dc, dp = self.down_shares, self.down_cost, self.down_principal
        if side == "Up":
            us += q
            uc += cost
            up += principal
        else:
            ds += q
            dc += cost
            dp += principal
        return min((us - uc) - dp, (ds - dc) - up)

    def is_two_sided(self) -> bool:
        return self.up_shares >= 1.0 and self.down_shares >= 1.0

    def is_naked(self) -> bool:
        return bool(self.trades) and not self.is_two_sided()

    def naked_cover_side(self) -> str | None:
        if self.up_shares >= 1.0 and self.down_shares < 1.0:
            return "Down"
        if self.down_shares >= 1.0 and self.up_shares < 1.0:
            return "Up"
        return None

    def share_ratio(self) -> float:
        if not self.trades:
            return 0.0
        if self.up_shares < 1.0 or self.down_shares < 1.0:
            return math.inf
        return max(self.up_shares / self.down_shares, self.down_shares / self.up_shares)

    def ratio_cover_need(self, max_ratio: float) -> tuple[str, float] | None:
        if max_ratio <= 0.0 or not self.trades:
            return None
        if self.up_shares > self.down_shares * max_ratio:
            return ("Down", max(0.0, math.ceil(self.up_shares / max_ratio - self.down_shares)))
        if self.down_shares > self.up_shares * max_ratio:
            return ("Up", max(0.0, math.ceil(self.down_shares / max_ratio - self.up_shares)))
        return None


def load_zticks(path: Path) -> dict[str, list[dict[str, Any]]]:
    out: dict[str, list[dict[str, Any]]] = collections.defaultdict(list)
    for row in base.iter_jsonl(path):
        if row.get("phase") != "z_tick":
            continue
        slug = row.get("market") or row.get("slug")
        if not slug:
            continue
        row = dict(row)
        row["ts"] = base.fnum(row.get("ts"))
        row["seconds_left"] = base.fnum(row.get("seconds_left"))
        row["up_ask"] = base.fnum(row.get("up_ask"))
        row["dn_ask"] = base.fnum(row.get("dn_ask"))
        row["z"] = base.fnum(row.get("z"))
        row["p_up"] = base.fnum(row.get("p_up"))
        out[str(slug)].append(row)
    return {slug: sorted(rows, key=lambda r: (r["ts"], r.get("_line", 0))) for slug, rows in out.items()}


def winners_from_state(state: dict[str, dict[str, Any]]) -> dict[str, str]:
    return {
        slug: str(pos.get("winner"))
        for slug, pos in state.items()
        if pos.get("winner") in ("Up", "Down")
    }


def original_pnls(state: dict[str, dict[str, Any]]) -> list[tuple[float, str, float]]:
    rows = []
    for slug, pos in state.items():
        if pos.get("winner") in ("Up", "Down") and pos.get("trades") and pos.get("realized_pnl") is not None:
            rows.append((base.fnum(pos.get("end_ts")), slug, base.fnum(pos.get("realized_pnl"))))
    return sorted(rows)


def market_end_ts(rows: list[dict[str, Any]]) -> float:
    vals = [base.fnum(r.get("ts")) + base.fnum(r.get("seconds_left")) for r in rows]
    return max(vals) if vals else 0.0


def dir_p(row: dict[str, Any], side: str) -> float:
    p_up = base.fnum(row.get("p_up"))
    return p_up if side == "Up" else 1.0 - p_up


def side_ask(row: dict[str, Any], side: str) -> float:
    return base.fnum(row.get("up_ask" if side == "Up" else "dn_ask"))


def opp_ask(row: dict[str, Any], side: str) -> float:
    return base.fnum(row.get("dn_ask" if side == "Up" else "up_ask"))


def calc_qty(pos: Pos, main_dir: str, side: str, price: float, target: float, maxloss: float) -> float:
    cur = pos.pnl_decision(side)
    denom = 1.0 - base.full_cost_per_share(price)
    if denom <= 0.001:
        return 0.0
    goal = target if side == main_dir else -maxloss
    return max(0.0, math.ceil((goal - cur) / denom))


def normal_order_shares(shares: float, price: float, min_order_usdc: float = 0.0) -> float:
    q = math.floor(shares + 1e-9)
    if min_order_usdc > 0 and price > 0:
        q = max(q, math.ceil(min_order_usdc / price - 1e-9))
    return float(q)


def allow_buy(pos: Pos, side: str, shares: float, price: float, args: argparse.Namespace) -> bool:
    q = normal_order_shares(shares, price, args.min_order_usdc)
    if q < 1:
        return False
    if args.max_exposure > 0:
        exposure = pos.up_cost + pos.down_cost
        if exposure + base.full_cost_per_share(price) * q > args.max_exposure:
            return False
    if args.hard_max_loss > 0:
        if pos.worst_if_add(side, q, price) < -args.hard_max_loss:
            return False
    return True


def active_qty(args: argparse.Namespace, seconds_left: int) -> float:
    risk_qty = getattr(args, "risk_qty", 0.0)
    risk_start = getattr(args, "risk_start_seconds_left", 0)
    if risk_qty > 0.0 and risk_start > 0 and seconds_left <= risk_start:
        return risk_qty
    return args.qty


def run_market(slug: str, rows: list[dict[str, Any]], winner: str, args: argparse.Namespace) -> tuple[float, Pos, list[str]]:
    pos = Pos()
    notes: list[str] = []
    main_dir: str | None = None
    trend_dir: str | None = None
    locked = False
    rescued = False
    up_chase: set[int] = set()
    dn_chase: set[int] = set()
    up_dip: set[int] = set()
    dn_dip: set[int] = set()
    up_trend: set[int] = set()
    dn_trend: set[int] = set()

    def lock_ready() -> bool:
        if main_dir is None:
            return False
        opp = "Down" if main_dir == "Up" else "Up"
        lock_max_loss = args.lock_max_loss if args.lock_max_loss > 0 else args.max_loss
        ready = pos.pnl_decision(main_dir) >= args.target_win and pos.pnl_decision(opp) >= -lock_max_loss
        if args.require_two_sided_lock and not pos.is_two_sided():
            return False
        return ready

    def future_row(start_idx: int, target_ts: float) -> dict[str, Any]:
        if target_ts <= base.fnum(rows[start_idx].get("ts")):
            return rows[start_idx]
        for nxt in rows[start_idx:]:
            if base.fnum(nxt.get("ts")) >= target_ts:
                return nxt
        return rows[-1]

    def context_rows(start_idx: int, since_ts: float, through_ts: float) -> list[dict[str, Any]]:
        out: list[dict[str, Any]] = []
        for prev in reversed(rows[: start_idx + 1]):
            prev_ts = base.fnum(prev.get("ts"))
            if prev_ts < since_ts:
                break
            if prev_ts <= through_ts:
                out.append(prev)
        out.reverse()
        return out

    def enforce_share_ratio(row: dict[str, Any], ts: float) -> None:
        max_ratio = getattr(args, "max_share_ratio", 0.0)
        if max_ratio <= 0.0:
            return
        step_qty = active_qty(args, int(base.fnum(row.get("seconds_left"))))
        for _ in range(20):
            cover = pos.ratio_cover_need(max_ratio)
            if not cover:
                return
            cover_side, need = cover
            ask = side_ask(row, cover_side)
            if not (0.0 < ask < 1.0):
                return
            this = min(need, step_qty)
            if not allow_buy(pos, cover_side, this, ask, args):
                return
            before = len(pos.trades)
            pos.add(cover_side, this, ask, "accum_ratio_cover", ts, args.min_order_usdc)
            if len(pos.trades) == before:
                return

    def buy(row: dict[str, Any], side: str, shares: float, price: float, phase: str, ts: float) -> bool:
        if not allow_buy(pos, side, shares, price, args):
            return False
        before = len(pos.trades)
        pos.add(side, shares, price, phase, ts, args.min_order_usdc)
        if len(pos.trades) == before:
            return False
        enforce_share_ratio(row, ts)
        return True

    def rescue_whipsaw_blocked(row_idx: int, ts: float, side: str, price: float) -> bool:
        min_price = getattr(args, "rescue_whipsaw_min_price", 0.0)
        if min_price <= 0.0 or price < min_price:
            return False
        checks: list[bool] = []
        max_secs = getattr(args, "rescue_whipsaw_max_lookback_seconds", 0.0)
        min_side_max = getattr(args, "rescue_whipsaw_min_side_max", 0.0)
        if max_secs > 0.0 and min_side_max > 0.0:
            vals = [side_ask(r, side) for r in context_rows(row_idx, ts - max_secs, ts)]
            checks.append(bool(vals) and max(vals) >= min_side_max)
        range_secs = getattr(args, "rescue_whipsaw_range_seconds", 0.0)
        min_side_range = getattr(args, "rescue_whipsaw_min_side_range", 0.0)
        if range_secs > 0.0 and min_side_range > 0.0:
            vals = [side_ask(r, side) for r in context_rows(row_idx, ts - range_secs, ts)]
            checks.append(bool(vals) and max(vals) - min(vals) >= min_side_range)
        return bool(checks) and all(checks)

    def cover_naked(row: dict[str, Any], ts: float) -> None:
        if main_dir is None:
            return
        cover_side = pos.naked_cover_side()
        if not cover_side:
            return
        ask = side_ask(row, cover_side)
        need = args.cover_min_shares
        if args.cover_to_max_loss:
            need = max(need, calc_qty(pos, main_dir, cover_side, ask, args.target_win, args.max_loss))
        step_qty = active_qty(args, int(base.fnum(row.get("seconds_left"))))
        for _ in range(50):
            if need < 1.0:
                break
            this = min(need, step_qty)
            if not allow_buy(pos, cover_side, this, ask, args):
                break
            buy(row, cover_side, this, ask, "accum_cover", ts)
            if not args.cover_to_max_loss:
                break
            need = calc_qty(pos, main_dir, cover_side, ask, args.target_win, args.max_loss)

    def try_rescue(
        row: dict[str, Any],
        ts: float,
        seconds_left: int,
        up_ask: float,
        dn_ask: float,
        *,
        row_idx: int,
        locked_context: bool = False,
    ) -> bool:
        nonlocal locked, rescued
        if rescued or seconds_left >= args.rescue_secs:
            return False
        fired: tuple[str, float] | None = None
        if args.rescue_lo < up_ask < args.rescue_hi:
            fired = ("Up", up_ask)
        elif args.rescue_lo < dn_ask < args.rescue_hi:
            fired = ("Down", dn_ask)
        if not fired:
            return False

        side, price = fired
        if rescue_whipsaw_blocked(row_idx, ts, side, price):
            rescued = True
            if args.rescue_whipsaw_lock:
                locked = True
            return True
        recheck_min_price = getattr(args, "rescue_recheck_min_price", 0.0)
        do_recheck = args.rescue_recheck and (
            recheck_min_price <= 0.0 or price >= recheck_min_price
        )
        if locked_context and pos.pnl_settle(side) > args.rescue_locked_side_pnl_below:
            return False
        reasons = []
        if args.rescue_min_seconds_left > 0 and seconds_left < args.rescue_min_seconds_left:
            reasons.append("late")
        if args.rescue_max_seconds_left > 0 and seconds_left > args.rescue_max_seconds_left:
            reasons.append("early")
        if args.rescue_min_p_side > 0 and dir_p(row, side) < args.rescue_min_p_side:
            reasons.append("low_p")
        if args.rescue_max_stale_ask > 0 and price - side_ask(row, side) > args.rescue_max_stale_ask:
            reasons.append("stale")
        if args.rescue_max_pre_trades > 0 and len(pos.trades) > args.rescue_max_pre_trades:
            reasons.append("pre_trades")
        rescued = True
        if not reasons:
            for step in range(50):
                cur_row = row
                recheck_step = getattr(args, "rescue_recheck_step_seconds", 0.0)
                if do_recheck and recheck_step > 0.0:
                    cur_row = future_row(row_idx, ts + step * recheck_step)
                cur_ts = base.fnum(cur_row.get("ts"))
                cur_price = side_ask(cur_row, side) if do_recheck else price
                if do_recheck and not (args.rescue_lo < cur_price < args.rescue_hi):
                    break
                full_cost = base.full_cost_per_share(cur_price)
                denom = 1.0 - full_cost
                if denom <= 0.001:
                    break
                target_need = max(0.0, math.ceil((args.rescue_goal - pos.pnl_settle(side)) / denom))
                risk_need = math.inf
                if args.rescue_max_worst_loss > 0:
                    lose_side = "Down" if side == "Up" else "Up"
                    risk_need = max(0.0, math.floor(
                        (pos.pnl_settle(lose_side) + args.rescue_max_worst_loss) / full_cost
                    ))
                need = min(target_need, risk_need)
                if need < 1.0:
                    break
                step_qty = active_qty(args, int(base.fnum(cur_row.get("seconds_left"))))
                this = min(need, step_qty)
                if args.rescue_max_shares > 0:
                    rescue_shares = sum(t.shares for t in pos.trades if t.phase == "accum_rescue")
                    rescue_room = args.rescue_max_shares - rescue_shares
                    if rescue_room < 1.0:
                        break
                    this = min(this, rescue_room)
                q = normal_order_shares(this, cur_price, args.min_order_usdc)
                if args.rescue_max_worst_loss > 0 and q > risk_need:
                    break
                if args.rescue_max_shares > 0 and q > rescue_room:
                    break
                if not allow_buy(pos, side, this, cur_price, args):
                    break
                buy(cur_row, side, this, cur_price, "accum_rescue", cur_ts)
                if target_need <= step_qty or need <= step_qty:
                    break
        locked = True
        return True

    for row_idx, row in enumerate(rows):
        seconds_left = int(base.fnum(row.get("seconds_left")))
        force_stop = seconds_left <= args.force_seconds
        up_ask = base.fnum(row.get("up_ask"))
        dn_ask = base.fnum(row.get("dn_ask"))
        ts = base.fnum(row.get("ts"))
        if not (0.0 < up_ask < 1.0 and 0.0 < dn_ask < 1.0):
            continue

        if main_dir is None:
            if force_stop:
                continue
            if args.entry_min_seconds_left > 0 and seconds_left < args.entry_min_seconds_left:
                continue
            z = base.fnum(row.get("z"))
            if z >= args.entry_z:
                side = "Up"
                ask = up_ask
            elif z <= -args.entry_z:
                side = "Down"
                ask = dn_ask
            else:
                continue
            if ask > args.entry_max_ask:
                continue
            step_qty = active_qty(args, seconds_left)
            if buy(row, side, step_qty, ask, "accum_first", ts):
                main_dir = side
                if args.cover_after_first:
                    cover_naked(row, ts)
                    if lock_ready():
                        locked = True
            continue

        if args.cover_naked_secs > 0 and seconds_left <= args.cover_naked_secs:
            cover_naked(row, ts)
            if lock_ready():
                locked = True

        if locked:
            if getattr(args, "rescue_on_locked", False) and try_rescue(
                row, ts, seconds_left, up_ask, dn_ask, row_idx=row_idx, locked_context=True
            ):
                continue
            continue

        if not force_stop and args.trend_confirm_ask > 0 and trend_dir is None:
            if up_ask >= args.trend_confirm_ask and dn_ask <= args.trend_confirm_opp_ask:
                trend_dir = "Up"
            elif dn_ask >= args.trend_confirm_ask and up_ask <= args.trend_confirm_opp_ask:
                trend_dir = "Down"
            if trend_dir and args.trend_switch_main:
                main_dir = trend_dir
                notes.append(f"{base.bj_time(ts)} trend {trend_dir}")

        if lock_ready():
            locked = True
            continue

        risk_start_seconds_left = getattr(args, "risk_start_seconds_left", 0)
        if risk_start_seconds_left > 0 and seconds_left > risk_start_seconds_left:
            continue

        if not force_stop:
            step_qty = active_qty(args, seconds_left)
            for side in ["Up", "Down"]:
                ask = side_ask(row, side)
                chased = up_chase if side == "Up" else dn_chase
                if args.block_counter_chase:
                    if side == "Up" and dn_chase:
                        continue
                    if side == "Down" and up_chase:
                        continue
                for k, lv in enumerate(args.chase_levels):
                    if k in chased or ask < lv:
                        continue
                    if buy(row, side, step_qty, ask, "accum_chase", ts):
                        chased.add(k)
                    if lock_ready():
                        locked = True
                        break
                if locked:
                    break
            if locked:
                continue

            if trend_dir:
                ask = side_ask(row, trend_dir)
                followed = up_trend if trend_dir == "Up" else dn_trend
                for k, lv in enumerate(args.trend_follow_levels):
                    if k in followed or ask < lv:
                        continue
                    if buy(row, trend_dir, step_qty, ask, "accum_trend", ts):
                        followed.add(k)
                    if lock_ready():
                        locked = True
                        break
                if locked:
                    continue

        if try_rescue(row, ts, seconds_left, up_ask, dn_ask, row_idx=row_idx):
            continue
        if force_stop:
            continue

        for side in ["Up", "Down"]:
            if getattr(args, "block_dip_against_chase", False):
                if side == "Up" and dn_chase:
                    continue
                if side == "Down" and up_chase:
                    continue
            if args.block_counter_dip_on_trend and trend_dir and side != trend_dir:
                continue
            ask = side_ask(row, side)
            dipped = up_dip if side == "Up" else dn_dip
            for j, lv in enumerate(args.dip_levels):
                if j in dipped or ask > lv:
                    continue
                dipped.add(j)
                for _ in range(50):
                    need = calc_qty(pos, main_dir, side, ask, args.target_win, args.max_loss)
                    risk_need = math.inf
                    if args.dip_max_worst_loss > 0:
                        lose_side = "Down" if side == "Up" else "Up"
                        full_cost = base.full_cost_per_share(ask)
                        risk_need = max(0.0, math.floor(
                            (pos.pnl_settle(lose_side) + args.dip_max_worst_loss) / full_cost
                        ))
                        need = min(need, risk_need)
                    if need < 1.0:
                        break
                    step_qty = active_qty(args, seconds_left)
                    this = min(need, step_qty)
                    dip_room = math.inf
                    if args.dip_max_shares > 0:
                        dip_shares = sum(t.shares for t in pos.trades if t.phase == "accum_dip")
                        dip_room = args.dip_max_shares - dip_shares
                        if dip_room < 1.0:
                            break
                        this = min(this, dip_room)
                    q = normal_order_shares(this, ask, args.min_order_usdc)
                    if args.dip_max_worst_loss > 0 and q > risk_need:
                        break
                    if args.dip_max_shares > 0 and q > dip_room:
                        break
                    if not allow_buy(pos, side, this, ask, args):
                        break
                    buy(row, side, this, ask, "accum_dip", ts)
                    if lock_ready():
                        locked = True
                        break
                    if need <= step_qty:
                        break
                if locked:
                    break
            if locked:
                break

    return pos.pnl_settle(winner), pos, notes


def summarize(label: str, rows: list[tuple[float, str, float, Pos]]) -> dict[str, Any]:
    pnls = [r[2] for r in rows]
    exposures = [r[3].up_cost + r[3].down_cost for r in rows]
    share_ratios = [r[3].share_ratio() for r in rows]
    two_sided = sum(1 for r in rows if r[3].is_two_sided())
    naked = sum(1 for r in rows if r[3].is_naked())
    return {
        "label": label,
        "markets": len(pnls),
        "pnl": sum(pnls),
        "avg": base.mean(pnls),
        "median": statistics.median(pnls) if pnls else 0.0,
        "win_rate": sum(1 for x in pnls if x >= 0.0) / len(pnls) if pnls else 0.0,
        "mdd": base.max_drawdown(pnls),
        "worst": min(pnls) if pnls else 0.0,
        "best": max(pnls) if pnls else 0.0,
        "trades": sum(len(r[3].trades) for r in rows),
        "two_sided_markets": two_sided,
        "naked_markets": naked,
        "two_sided_rate": two_sided / len(rows) if rows else 0.0,
        "rescue_markets": sum(1 for r in rows if any(t.phase == "accum_rescue" for t in r[3].trades)),
        "cover_markets": sum(1 for r in rows if any(t.phase == "accum_cover" for t in r[3].trades)),
        "ratio_cover_markets": sum(1 for r in rows if any(t.phase == "accum_ratio_cover" for t in r[3].trades)),
        "avg_exposure": base.mean(exposures),
        "max_exposure": max(exposures) if exposures else 0.0,
        "max_share_ratio": max(share_ratios) if share_ratios else 0.0,
    }


def with_split_metrics(r: dict[str, Any], rows: list[tuple[float, str, float, Pos]]) -> dict[str, Any]:
    mid = len(rows) // 2
    first = summarize(r["label"] + " first", rows[:mid])
    second = summarize(r["label"] + " second", rows[mid:])
    by_day: dict[str, list[tuple[float, str, float, Pos]]] = collections.defaultdict(list)
    for row in rows:
        by_day[base.bj_date(row[0])].append(row)
    day_reports = {day: summarize(day, day_rows) for day, day_rows in sorted(by_day.items())}
    r = dict(r)
    r["first_pnl"] = first["pnl"]
    r["second_pnl"] = second["pnl"]
    r["first_worst"] = first["worst"]
    r["second_worst"] = second["worst"]
    r["min_half_pnl"] = min(first["pnl"], second["pnl"])
    r["day_pnls"] = {day: rep["pnl"] for day, rep in day_reports.items()}
    r["min_day_pnl"] = min(r["day_pnls"].values()) if r["day_pnls"] else 0.0
    r["day_worsts"] = {day: rep["worst"] for day, rep in day_reports.items()}
    return r


def print_summary(r: dict[str, Any]) -> None:
    print(
        f"{r['label']:<44s} markets={r['markets']:>4} pnl={r['pnl']:>+9.2f} "
        f"avg={r['avg']:>+7.3f} med={r['median']:>+7.2f} "
        f"mdd={r['mdd']:>8.2f} worst={r['worst']:>+8.2f} "
        f"win={100*r['win_rate']:>5.1f}% trades={r['trades']:>5} naked_m={r['naked_markets']:>4} "
        f"two_m={r['two_sided_markets']:>4} cover_m={r['cover_markets']:>3} rescue_m={r['rescue_markets']:>3} "
        f"ratioMax={r['max_share_ratio']:>5.2f} exp_avg={r['avg_exposure']:>6.1f} exp_max={r['max_exposure']:>6.1f}"
    )


def print_split_summary(r: dict[str, Any]) -> None:
    days = " ".join(f"{day}:{pnl:+.1f}" for day, pnl in r.get("day_pnls", {}).items())
    print(
        f"{r['label']:<44s} pnl={r['pnl']:>+8.2f} "
        f"h1={r.get('first_pnl', 0.0):>+7.1f} h2={r.get('second_pnl', 0.0):>+7.1f} "
        f"minDay={r.get('min_day_pnl', 0.0):>+7.1f} "
        f"mdd={r['mdd']:>7.2f} worst={r['worst']:>+7.2f} naked_m={r['naked_markets']:>4} "
        f"two_m={r['two_sided_markets']:>4} cover_m={r['cover_markets']:>3} rescue_m={r['rescue_markets']:>3} "
        f"ratioMax={r['max_share_ratio']:>5.2f} "
        f"{days}"
    )


def prepare_markets(data: Path, universe: str, min_ticks: int) -> tuple[
    list[tuple[float, str, str, list[dict[str, Any]]]],
    dict[str, dict[str, Any]],
]:
    state = base.load_state(data / "quant_state.json")
    events = list(base.iter_jsonl(data / "data" / "quant_signals.jsonl"))
    event_winners = base.settlement_winners(events)
    state_winners = winners_from_state(state)
    winners = {**event_winners, **state_winners}
    zticks = load_zticks(data / "data" / "quant_signals.jsonl")
    markets: list[tuple[float, str, str, list[dict[str, Any]]]] = []
    if universe == "ledger":
        candidates = [(end_ts, slug) for end_ts, slug, _old in original_pnls(state)]
    else:
        candidates = [(market_end_ts(rows), slug) for slug, rows in zticks.items()]
    for end_ts, slug in sorted(candidates):
        if slug not in zticks or slug not in winners:
            continue
        if len(zticks[slug]) < min_ticks:
            continue
        markets.append((end_ts, slug, winners[slug], zticks[slug]))
    return markets, state


def run_prepared(
    markets: list[tuple[float, str, str, list[dict[str, Any]]]],
    args: argparse.Namespace,
) -> list[tuple[float, str, float, Pos]]:
    rows: list[tuple[float, str, float, Pos]] = []
    for end_ts, slug, winner, ticks in markets:
        pnl, pos, _notes = run_market(slug, ticks, winner, args)
        if pos.trades:
            rows.append((end_ts, slug, pnl, pos))
    rows.sort(key=lambda r: r[0])
    return rows


def run_all(data: Path, args: argparse.Namespace) -> list[tuple[float, str, float, Pos]]:
    markets, _state = prepare_markets(data, args.universe, args.min_ticks)
    return run_prepared(markets, args)


def add_args(ap: argparse.ArgumentParser) -> None:
    ap.add_argument("path", help="extracted data dir or bundle")
    ap.add_argument("--universe", choices=["ledger", "all"], default="ledger",
                    help="ledger=replay markets traded by original accum ledger; all=replay all markets with z_tick+settlement")
    ap.add_argument("--min-ticks", type=int, default=1, help="Minimum z_tick rows required for a market in --universe all")
    ap.add_argument("--qty", type=float, default=10.0)
    ap.add_argument("--min-order-usdc", type=float, default=0.0,
                    help="Raise shares so price*shares is at least this amount. Use 1 for Polymarket-sized orders.")
    ap.add_argument("--entry-z", type=float, default=0.15)
    ap.add_argument("--entry-max-ask", type=float, default=1.0)
    ap.add_argument("--entry-min-seconds-left", type=int, default=0)
    ap.add_argument("--force-seconds", type=int, default=15)
    ap.add_argument("--chase-levels", default="0.62,0.65,0.68,0.70")
    ap.add_argument("--dip-levels", default="0.25,0.20")
    ap.add_argument("--dip-max-shares", type=float, default=0.0,
                    help="Cap total accum_dip shares per market. 0 disables the cap.")
    ap.add_argument("--dip-max-worst-loss", type=float, default=0.0,
                    help="Cap dip size so the losing-side settlement PnL stays above -this amount. 0 disables the cap.")
    ap.add_argument("--target-win", type=float, default=12.0)
    ap.add_argument("--max-loss", type=float, default=7.0)
    ap.add_argument("--rescue-secs", type=int, default=100)
    ap.add_argument("--rescue-min-seconds-left", type=int, default=0)
    ap.add_argument("--rescue-max-seconds-left", type=int, default=0)
    ap.add_argument("--rescue-lo", type=float, default=0.78)
    ap.add_argument("--rescue-hi", type=float, default=0.83)
    ap.add_argument("--rescue-goal", type=float, default=10.0)
    ap.add_argument("--rescue-recheck", action=argparse.BooleanOptionalAction, default=False)
    ap.add_argument("--rescue-recheck-step-seconds", type=float, default=0.0,
                    help="With --rescue-recheck, advance this many historical seconds between split rescue buys. 0 keeps same-tick legacy replay.")
    ap.add_argument("--rescue-recheck-min-price", type=float, default=0.0,
                    help="Only apply --rescue-recheck when the initial rescue price is at least this value. 0 applies to all rescue prices.")
    ap.add_argument("--rescue-whipsaw-min-price", type=float, default=0.0,
                    help="If >0, block rescue when initial price is at least this value and whipsaw context checks pass.")
    ap.add_argument("--rescue-whipsaw-max-lookback-seconds", type=float, default=0.0,
                    help="Lookback window for the pre-rescue side max check.")
    ap.add_argument("--rescue-whipsaw-min-side-max", type=float, default=0.0,
                    help="Require the rescue side to have reached at least this ask in the max lookback window.")
    ap.add_argument("--rescue-whipsaw-range-seconds", type=float, default=0.0,
                    help="Lookback window for the pre-rescue side range check.")
    ap.add_argument("--rescue-whipsaw-min-side-range", type=float, default=0.0,
                    help="Require rescue side ask range in the range window to be at least this amount.")
    ap.add_argument("--rescue-whipsaw-lock", action=argparse.BooleanOptionalAction, default=True,
                    help="When the whipsaw filter blocks rescue, mark the market locked. If disabled, only disables rescue for the market.")
    ap.add_argument("--rescue-max-stale-ask", type=float, default=0.0)
    ap.add_argument("--rescue-min-p-side", type=float, default=0.0)
    ap.add_argument("--rescue-max-pre-trades", type=int, default=0)
    ap.add_argument("--rescue-max-shares", type=float, default=0.0,
                    help="Cap total accum_rescue shares per market. 0 disables the cap.")
    ap.add_argument("--rescue-max-worst-loss", type=float, default=0.0,
                    help="Cap rescue size so the losing-side settlement PnL stays above -this amount. 0 disables the cap.")
    ap.add_argument("--rescue-on-locked", action=argparse.BooleanOptionalAction, default=False,
                    help="Let late rescue run even after an earlier pnl lock. Default keeps old accum behavior.")
    ap.add_argument("--rescue-locked-side-pnl-below", type=float, default=0.0,
                    help="With --rescue-on-locked, only rescue when the fired side's current settlement PnL is <= this value.")
    ap.add_argument("--trend-confirm-ask", type=float, default=0.0)
    ap.add_argument("--trend-confirm-opp-ask", type=float, default=0.35)
    ap.add_argument("--trend-follow-levels", default="")
    ap.add_argument("--trend-switch-main", action=argparse.BooleanOptionalAction, default=True)
    ap.add_argument("--block-counter-dip-on-trend", action=argparse.BooleanOptionalAction, default=True)
    ap.add_argument("--block-dip-against-chase", action=argparse.BooleanOptionalAction, default=False)
    ap.add_argument("--block-counter-chase", action=argparse.BooleanOptionalAction, default=False,
                    help="After one side has been chased, do not chase the opposite side in the same market.")
    ap.add_argument("--cover-naked-secs", type=int, default=0,
                    help="If a market still has only one side by this T-minus second, buy the opposite side.")
    ap.add_argument("--cover-min-shares", type=float, default=5.0)
    ap.add_argument("--cover-to-max-loss", action=argparse.BooleanOptionalAction, default=True)
    ap.add_argument("--cover-after-first", action=argparse.BooleanOptionalAction, default=False)
    ap.add_argument("--require-two-sided-lock", action=argparse.BooleanOptionalAction, default=False)
    ap.add_argument("--hard-max-loss", type=float, default=0.0)
    ap.add_argument("--lock-max-loss", type=float, default=0.0,
                    help="Use this smaller adverse PnL threshold for declaring the market locked. 0 reuses --max-loss.")
    ap.add_argument("--max-exposure", type=float, default=0.0)
    ap.add_argument("--max-share-ratio", type=float, default=0.0,
                    help="If >0, auto-buy the smaller side after each fill so shares stay within this ratio.")
    ap.add_argument("--risk-start-seconds-left", type=int, default=0,
                    help="If >0, only allow chase/trend/rescue/dip risk adds once seconds_left <= this value. Initial entry/cover still run.")
    ap.add_argument("--grid", action="store_true")
    ap.add_argument("--grid-splits", action="store_true")
    ap.add_argument("--shape-grid", action="store_true", help="Scan chase/dip level shapes with current other parameters")
    ap.add_argument("--show-worst", type=int, default=10)


def parse_vec(s: str) -> list[float]:
    return [float(x) for x in s.split(",") if x.strip()]


def main() -> int:
    ap = argparse.ArgumentParser()
    add_args(ap)
    args = ap.parse_args()
    args.chase_levels = parse_vec(args.chase_levels)
    args.dip_levels = parse_vec(args.dip_levels)
    args.trend_follow_levels = parse_vec(args.trend_follow_levels)
    data, tmp = base.resolve_data_path(args.path)
    try:
        markets, state = prepare_markets(data, args.universe, args.min_ticks)
        if args.shape_grid:
            orig = vars(args).copy()
            chase_shapes = [
                "0.62,0.65,0.68,0.70",
                "0.65,0.68,0.70",
                "0.68,0.70",
                "0.70",
                "0.72",
                "",
            ]
            dip_shapes = [
                "0.25,0.20",
                "0.25",
                "0.20",
                "0.18",
                "0.15",
                "0.20,0.15",
                "",
            ]
            combos = []
            for chase, dip in itertools.product(chase_shapes, dip_shapes):
                args.chase_levels = parse_vec(chase)
                args.dip_levels = parse_vec(dip)
                rows = run_prepared(markets, args)
                summary = with_split_metrics(
                    summarize(f"ch={chase or '-'} dip={dip or '-'}", rows),
                    rows,
                )
                combos.append(summary)
            combos.sort(key=lambda r: (r["min_day_pnl"], r["pnl"], r["worst"]), reverse=True)
            print("[shape grid by min day pnl]")
            for r in combos[:50]:
                print_split_summary(r)
            for k, v in orig.items():
                setattr(args, k, v)
            return 0

        if args.grid or args.grid_splits:
            orig = vars(args).copy()
            combos = []
            for min_sec, max_sec, stale, recheck in itertools.product(
                [0, 35, 40, 45, 50],
                [0, 85, 90, 95],
                [0.0, 0.03, 0.05, 0.08, 0.10],
                [False, True],
            ):
                if max_sec and min_sec and min_sec > max_sec:
                    continue
                args.rescue_min_seconds_left = min_sec
                args.rescue_max_seconds_left = max_sec
                args.rescue_max_stale_ask = stale
                args.rescue_recheck = recheck
                rows = run_prepared(markets, args)
                summary = summarize(f"min{min_sec} max{max_sec} stale{stale:.2f} re{int(recheck)}", rows)
                if args.grid_splits:
                    summary = with_split_metrics(summary, rows)
                combos.append(summary)
            combos.sort(key=lambda r: (r["pnl"], r["worst"], -r["mdd"]), reverse=True)
            print("[grid top pnl]")
            for r in combos[:40]:
                if args.grid_splits:
                    print_split_summary(r)
                else:
                    print_summary(r)
            print("\n[grid top with worst >= -80]")
            for r in [x for x in combos if x["worst"] >= -80][:40]:
                if args.grid_splits:
                    print_split_summary(r)
                else:
                    print_summary(r)
            if args.grid_splits:
                robust = [
                    x for x in combos
                    if x["worst"] >= -80 and x.get("min_half_pnl", -1e9) > 0 and x.get("min_day_pnl", -1e9) > 0
                ]
                robust.sort(key=lambda r: (r["min_day_pnl"], r["min_half_pnl"], r["pnl"], r["worst"]), reverse=True)
                print("\n[grid robust: worst >= -80 and every half/day positive]")
                for r in robust[:40]:
                    print_split_summary(r)
            for k, v in orig.items():
                setattr(args, k, v)
            return 0

        rows = run_prepared(markets, args)
        print_summary(summarize("sim accum", rows))
        old = [(e, s, p) for e, s, p in original_pnls(state)]
        old_summary_rows = [(e, s, p, Pos()) for e, s, p in old]
        print_summary(summarize("old ledger", old_summary_rows))
        by_slug = {slug: (end_ts, pnl, pos) for end_ts, slug, pnl, pos in rows}
        print("\n[worst simulated]")
        for end_ts, slug, pnl, pos in sorted(rows, key=lambda r: r[2])[:args.show_worst]:
            old_pnl = next((p for _e, s, p in old if s == slug), 0.0)
            phases = collections.Counter(t.phase for t in pos.trades)
            print(
                f"{pnl:>+8.2f} old={old_pnl:>+8.2f} {slug} {base.bj_time(end_ts)} "
                f"n={len(pos.trades)} {dict(phases)}"
            )
    finally:
        if tmp:
            tmp.cleanup()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
