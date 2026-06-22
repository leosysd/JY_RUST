#!/usr/bin/env python3
"""Scan late-confirmation taker overlays with bounded two-sided exposure.

Every market first buys both Up and Down at legal minimum notional.  If a later
tick shows one side clearly leading, the simulator adds to that side while
forcing the opposite side to remain at least a fixed fraction of the main side.

This is intentionally separate from the Rust live strategy: it is a research
overlay to test whether "last seconds already determined" can beat taker
spread/fees without reverting to a naked one-sided hold.
"""

from __future__ import annotations

import argparse
import collections
import itertools
import math
import statistics
from dataclasses import dataclass, field
from typing import Any

import analyze_backtest as base
import scan_no_naked_ratio as ratio
import sim_accum_taker as sim


@dataclass
class Trade:
    side: str
    shares: float
    price: float
    phase: str


@dataclass
class Pos:
    up_shares: float = 0.0
    down_shares: float = 0.0
    up_cost: float = 0.0
    down_cost: float = 0.0
    trades: list[Trade] = field(default_factory=list)

    def add(self, side: str, shares: float, price: float, phase: str) -> None:
        q = math.floor(shares + 1e-9)
        if q < 1:
            return
        cost = base.full_cost_per_share(price) * q
        if side == "Up":
            self.up_shares += q
            self.up_cost += cost
        else:
            self.down_shares += q
            self.down_cost += cost
        self.trades.append(Trade(side, float(q), price, phase))

    def shares(self, side: str) -> float:
        return self.up_shares if side == "Up" else self.down_shares

    def cost(self) -> float:
        return self.up_cost + self.down_cost

    def pnl(self, winner: str) -> float:
        payout = self.up_shares if winner == "Up" else self.down_shares
        return payout - self.cost()

    def two_sided(self) -> bool:
        return self.up_shares >= 1.0 and self.down_shares >= 1.0

    def max_share_ratio(self) -> float:
        return max(self.up_shares, self.down_shares) / max(min(self.up_shares, self.down_shares), 1.0)


def parse_nums(raw: str, cast=float):
    return [cast(x) for x in raw.split(",") if x.strip()]


def side_price(row: dict[str, Any], side: str) -> float:
    return base.fnum(row.get("up_ask" if side == "Up" else "dn_ask"))


def choose_entry_tick(ticks: list[dict[str, Any]], tminus: int, force_seconds: int) -> dict[str, Any] | None:
    return ratio.choose_tick(ticks, tminus, force_seconds)


def choose_confirm_tick(ticks: list[dict[str, Any]], tminus: int, force_seconds: int) -> dict[str, Any] | None:
    valid = [
        r for r in ticks
        if force_seconds < base.fnum(r.get("seconds_left")) <= tminus
        and 0.0 < base.fnum(r.get("up_ask")) < 1.0
        and 0.0 < base.fnum(r.get("dn_ask")) < 1.0
    ]
    if not valid:
        return None
    return max(valid, key=lambda r: base.fnum(r.get("seconds_left")))


def shares_for_usdc(price: float, usdc: float) -> float:
    if price <= 0:
        return 0.0
    return float(math.ceil(usdc / price - 1e-9))


def shares_for_request(price: float, shares: float, min_order_usdc: float) -> float:
    return ratio.shares_for_request(price, shares, min_order_usdc)


def add_base_pair(
    pos: Pos,
    up_px: float,
    dn_px: float,
    min_order_usdc: float,
    base_usdc: float,
    base_mode: str,
) -> None:
    min_usdc = max(min_order_usdc, base_usdc)
    up_q = shares_for_usdc(up_px, min_usdc)
    dn_q = shares_for_usdc(dn_px, min_usdc)
    if base_mode == "equal-shares":
        q = max(up_q, dn_q)
        pos.add("Up", q, up_px, "base_pair")
        pos.add("Down", q, dn_px, "base_pair")
        return
    pos.add("Up", up_q, up_px, "base_pair")
    pos.add("Down", dn_q, dn_px, "base_pair")


def run_market(
    entry: dict[str, Any] | None,
    confirm: dict[str, Any] | None,
    winner: str,
    base_usdc: float,
    base_mode: str,
    main_shares: float,
    min_main_ask: float,
    max_main_ask: float,
    max_opp_ask: float,
    min_spread: float,
    min_hedge_frac: float,
    min_order_usdc: float,
) -> tuple[float, Pos, dict[str, Any]]:
    pos = Pos()
    meta: dict[str, Any] = {
        "confirmed": False,
        "confirm_correct": None,
        "confirm_seconds": 0.0,
        "main": "",
        "main_px": 0.0,
        "hedge_px": 0.0,
    }
    if entry is None:
        return 0.0, pos, meta

    up0 = side_price(entry, "Up")
    dn0 = side_price(entry, "Down")
    add_base_pair(pos, up0, dn0, min_order_usdc, base_usdc, base_mode)

    if confirm is not None:
        up = side_price(confirm, "Up")
        dn = side_price(confirm, "Down")
        main = "Up" if up >= dn else "Down"
        hedge = "Down" if main == "Up" else "Up"
        main_px = side_price(confirm, main)
        hedge_px = side_price(confirm, hedge)
        spread = main_px - hedge_px
        if (
            main_px >= min_main_ask
            and (max_main_ask <= 0.0 or main_px <= max_main_ask)
            and hedge_px <= max_opp_ask
            and spread >= min_spread
        ):
            target_main = shares_for_request(main_px, main_shares, min_order_usdc)
            add_main = max(0.0, target_main - pos.shares(main))
            if add_main >= 1.0:
                pos.add(main, add_main, main_px, "late_confirm_main")
            target_hedge = math.ceil(pos.shares(main) * min_hedge_frac - 1e-9)
            add_hedge = max(0.0, target_hedge - pos.shares(hedge))
            if add_hedge >= 1.0:
                pos.add(hedge, add_hedge, hedge_px, "late_confirm_hedge")
            meta.update(
                {
                    "confirmed": True,
                    "confirm_correct": main == winner,
                    "confirm_seconds": base.fnum(confirm.get("seconds_left")),
                    "main": main,
                    "main_px": main_px,
                    "hedge_px": hedge_px,
                    "spread": spread,
                }
            )

    return pos.pnl(winner), pos, meta


def summarize(label: str, rows: list[tuple[float, str, float, Pos, dict[str, Any]]]) -> dict[str, Any]:
    pnls = [r[2] for r in rows]
    by_day: dict[str, float] = collections.defaultdict(float)
    for end_ts, _slug, pnl, _pos, _meta in rows:
        by_day[base.bj_date(end_ts)] += pnl
    confirmed = [r for r in rows if r[4]["confirmed"]]
    confirm_correct = sum(1 for r in confirmed if r[4]["confirm_correct"])
    costs = [r[3].cost() for r in rows]
    ratios = [r[3].max_share_ratio() for r in rows if r[3].two_sided()]
    return {
        "label": label,
        "markets": len(rows),
        "pnl": sum(pnls),
        "avg": statistics.fmean(pnls) if pnls else 0.0,
        "median": statistics.median(pnls) if pnls else 0.0,
        "win_rate": sum(1 for p in pnls if p >= 0) / len(pnls) if pnls else 0.0,
        "mdd": base.max_drawdown(pnls),
        "worst": min(pnls) if pnls else 0.0,
        "best": max(pnls) if pnls else 0.0,
        "two_m": sum(1 for _end, _slug, _pnl, pos, _meta in rows if pos.two_sided()),
        "confirmed_m": len(confirmed),
        "confirm_acc": confirm_correct / len(confirmed) if confirmed else 0.0,
        "avg_cost": statistics.fmean(costs) if costs else 0.0,
        "max_cost": max(costs) if costs else 0.0,
        "max_ratio": max(ratios) if ratios else 0.0,
        "min_day": min(by_day.values()) if by_day else 0.0,
        "day_pnls": dict(sorted(by_day.items())),
    }


def print_summary(r: dict[str, Any]) -> None:
    days = " ".join(f"{d}:{p:+.0f}" for d, p in r["day_pnls"].items())
    print(
        f"{r['label']:<72s} markets={r['markets']:>4} pnl={r['pnl']:>+9.2f} "
        f"avg={r['avg']:>+7.3f} med={r['median']:>+7.2f} win={100*r['win_rate']:>5.1f}% "
        f"mdd={r['mdd']:>8.2f} worst={r['worst']:>+8.2f} minDay={r['min_day']:>+8.1f} "
        f"two_m={r['two_m']:>4} conf_m={r['confirmed_m']:>4} confAcc={100*r['confirm_acc']:>5.1f}% "
        f"cost_avg={r['avg_cost']:>6.1f} cost_max={r['max_cost']:>6.1f} maxRatio={r['max_ratio']:>4.1f} "
        f"{days}"
    )


def run_combo(
    prepared: list[tuple[float, str, str, dict[int, dict[str, Any] | None], dict[int, dict[str, Any] | None]]],
    args: argparse.Namespace,
    entry_tminus: int,
    confirm_tminus: int,
    base_usdc: float,
    main_shares: float,
    min_main_ask: float,
    max_main_ask: float,
    max_opp_ask: float,
    min_spread: float,
    min_hedge_frac: float,
) -> tuple[dict[str, Any], list[tuple[float, str, float, Pos, dict[str, Any]]]]:
    rows = []
    for end_ts, slug, winner, entry_rows, confirm_rows in prepared:
        pnl, pos, meta = run_market(
            entry_rows.get(entry_tminus),
            confirm_rows.get(confirm_tminus),
            winner,
            base_usdc=base_usdc,
            base_mode=args.base_mode,
            main_shares=main_shares,
            min_main_ask=min_main_ask,
            max_main_ask=max_main_ask,
            max_opp_ask=max_opp_ask,
            min_spread=min_spread,
            min_hedge_frac=min_hedge_frac,
            min_order_usdc=args.min_order_usdc,
        )
        if pos.trades:
            rows.append((end_ts, slug, pnl, pos, meta))
    rows.sort(key=lambda r: r[0])
    label = (
        f"E{entry_tminus}/C{confirm_tminus} base{args.base_mode}:${base_usdc:g} main{main_shares:g} "
        f"ask={min_main_ask:g}-{max_main_ask:g} opp<={max_opp_ask:g} "
        f"spr>={min_spread:g} h>={min_hedge_frac:g}"
    )
    return summarize(label, rows), rows


def prepare_ticks(
    markets: list[tuple[float, str, str, list[dict[str, Any]]]],
    entry_times: list[int],
    confirm_times: list[int],
    force_seconds: int,
) -> list[tuple[float, str, str, dict[int, dict[str, Any] | None], dict[int, dict[str, Any] | None]]]:
    prepared = []
    for end_ts, slug, winner, ticks in markets:
        entry_rows = {t: choose_entry_tick(ticks, t, force_seconds) for t in entry_times}
        confirm_rows = {t: choose_confirm_tick(ticks, t, force_seconds) for t in confirm_times}
        prepared.append((end_ts, slug, winner, entry_rows, confirm_rows))
    return prepared


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("path")
    ap.add_argument("--universe", choices=["all", "ledger"], default="all")
    ap.add_argument("--min-ticks", type=int, default=1)
    ap.add_argument("--force-seconds", type=int, default=15)
    ap.add_argument("--min-order-usdc", type=float, default=1.0)
    ap.add_argument("--entry-times", default="300")
    ap.add_argument("--confirm-times", default="90,60,45,35,30,25,20,16")
    ap.add_argument("--base-usdc", default="1")
    ap.add_argument("--base-mode", choices=["usdc", "equal-shares"], default="usdc")
    ap.add_argument("--main-shares", default="10,20,30,50,75,100,150")
    ap.add_argument("--min-main-asks", default="0.75,0.78,0.80,0.83,0.85,0.88,0.90")
    ap.add_argument("--max-main-asks", default="0")
    ap.add_argument("--max-opp-asks", default="0.45,0.40,0.35,0.30,0.25")
    ap.add_argument("--min-spreads", default="0.20,0.30,0.40,0.50")
    ap.add_argument("--min-hedge-fracs", default="0.25,0.3333333333,0.5")
    ap.add_argument("--top", type=int, default=30)
    ap.add_argument("--show-worst", type=int, default=10)
    args = ap.parse_args()

    data, tmp = base.resolve_data_path(args.path)
    try:
        markets, _state = sim.prepare_markets(data, args.universe, args.min_ticks)
        entry_times = parse_nums(args.entry_times, int)
        confirm_times = parse_nums(args.confirm_times, int)
        prepared = prepare_ticks(markets, entry_times, confirm_times, args.force_seconds)
        reports = []
        for combo in itertools.product(
            entry_times,
            confirm_times,
            parse_nums(args.base_usdc, float),
            parse_nums(args.main_shares, float),
            parse_nums(args.min_main_asks, float),
            parse_nums(args.max_main_asks, float),
            parse_nums(args.max_opp_asks, float),
            parse_nums(args.min_spreads, float),
            parse_nums(args.min_hedge_fracs, float),
        ):
            report, rows = run_combo(prepared, args, *combo)
            reports.append((report, rows))

        reports.sort(key=lambda x: (x[0]["pnl"], x[0]["worst"]), reverse=True)
        print(
            f"[setup] markets={len(markets)} combos={len(reports)} "
            f"min_order=${args.min_order_usdc:g} base_mode={args.base_mode}"
        )
        print("\n[top pnl]")
        for r, _rows in reports[: args.top]:
            print_summary(r)

        print("\n[top positive mdd<=300]")
        safe = [x for x in reports if x[0]["pnl"] > 0 and x[0]["mdd"] <= 300]
        for r, _rows in safe[: args.top]:
            print_summary(r)

        if reports:
            best, best_rows = reports[0]
            print("\n[worst rows for best]")
            print_summary(best)
            for end_ts, slug, pnl, pos, meta in sorted(best_rows, key=lambda r: r[2])[: args.show_worst]:
                print(
                    f"{pnl:+8.2f} {slug} {base.bj_time(end_ts)} "
                    f"up={pos.up_shares:.0f} down={pos.down_shares:.0f} cost={pos.cost():.2f} "
                    f"conf={meta['confirmed']} main={meta['main']} "
                    f"px={meta['main_px']:.3f}/{meta['hedge_px']:.3f} "
                    f"T-{meta['confirm_seconds']:.0f}"
                )
        if safe:
            best, rows = safe[0]
            print("\n[worst rows for best safe]")
            print_summary(best)
            for end_ts, slug, pnl, pos, meta in sorted(rows, key=lambda r: r[2])[: args.show_worst]:
                print(
                    f"{pnl:+8.2f} {slug} {base.bj_time(end_ts)} "
                    f"up={pos.up_shares:.0f} down={pos.down_shares:.0f} cost={pos.cost():.2f} "
                    f"conf={meta['confirmed']} main={meta['main']} "
                    f"px={meta['main_px']:.3f}/{meta['hedge_px']:.3f} "
                    f"T-{meta['confirm_seconds']:.0f}"
                )
    finally:
        if tmp:
            tmp.cleanup()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
