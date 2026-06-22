#!/usr/bin/env python3
"""Scan every-market no-naked taker ratios.

Each market buys a directional main leg plus a legal opposite hedge leg at the
same recorded z_tick. This keeps the original accum signal surface (z_tick,
p_up, Up/Down asks) but forbids naked single-side settlement exposure.
"""

from __future__ import annotations

import argparse
import collections
import itertools
import math
import statistics
from pathlib import Path
from typing import Any, Callable

import analyze_backtest as base
import sim_accum_taker as sim


def parse_nums(raw: str, cast=float):
    return [cast(x) for x in raw.split(",") if x.strip()]


def choose_tick(ticks: list[dict[str, Any]], tminus: int, force_seconds: int) -> dict[str, Any] | None:
    valid = [
        r for r in ticks
        if base.fnum(r.get("seconds_left")) > force_seconds
        and 0.0 < base.fnum(r.get("up_ask")) < 1.0
        and 0.0 < base.fnum(r.get("dn_ask")) < 1.0
    ]
    if not valid:
        return None
    after = [r for r in valid if base.fnum(r.get("seconds_left")) <= tminus]
    if after:
        return max(after, key=lambda r: base.fnum(r.get("seconds_left")))
    return min(valid, key=lambda r: base.fnum(r.get("seconds_left")))


def side_price(row: dict[str, Any], side: str) -> float:
    return base.fnum(row.get("up_ask" if side == "Up" else "dn_ask"))


def shares_for_notional(price: float, notional: float) -> float:
    if price <= 0:
        return 0.0
    return float(math.ceil(notional / price - 1e-9))


def shares_for_request(price: float, requested_shares: float, min_usdc: float) -> float:
    q = math.floor(requested_shares + 1e-9)
    if price > 0 and min_usdc > 0:
        q = max(q, math.ceil(min_usdc / price - 1e-9))
    return float(q)


def direction(row: dict[str, Any], mode: str) -> str:
    up = base.fnum(row.get("up_ask"))
    dn = base.fnum(row.get("dn_ask"))
    z = base.fnum(row.get("z"))
    p_up = base.fnum(row.get("p_up"))
    if mode == "expensive":
        return "Up" if up >= dn else "Down"
    if mode == "cheap":
        return "Up" if up < dn else "Down"
    if mode == "z":
        return "Up" if z >= 0 else "Down"
    if mode == "p":
        return "Up" if p_up >= 0.5 else "Down"
    if mode == "accum_fallback_exp":
        if z >= 0.15:
            return "Up"
        if z <= -0.15:
            return "Down"
        return "Up" if up >= dn else "Down"
    if mode == "accum_fallback_p":
        if z >= 0.15:
            return "Up"
        if z <= -0.15:
            return "Down"
        return "Up" if p_up >= 0.5 else "Down"
    raise ValueError(f"unknown direction mode: {mode}")


def run_combo(
    markets: list[tuple[float, str, str, list[dict[str, Any]]]],
    tminus: int,
    mode: str,
    main_shares: float,
    hedge_usdc: float,
    force_seconds: int,
    min_order_usdc: float,
    min_hedge_share_frac: float,
) -> list[tuple[float, str, float, dict[str, Any]]]:
    out = []
    for end_ts, slug, winner, ticks in markets:
        row = choose_tick(ticks, tminus, force_seconds)
        if row is None:
            continue
        main = direction(row, mode)
        hedge = "Down" if main == "Up" else "Up"
        main_px = side_price(row, main)
        hedge_px = side_price(row, hedge)
        main_q = shares_for_request(main_px, main_shares, min_order_usdc)
        hedge_q = max(
            shares_for_notional(hedge_px, max(min_order_usdc, hedge_usdc)),
            math.ceil(main_q * min_hedge_share_frac - 1e-9),
        )
        if main_q < 1 or hedge_q < 1:
            continue
        cost = base.full_cost_per_share(main_px) * main_q + base.full_cost_per_share(hedge_px) * hedge_q
        payout = main_q if winner == main else hedge_q
        pnl = payout - cost
        out.append((
            end_ts,
            slug,
            pnl,
            {
                "main": main,
                "hedge": hedge,
                "main_px": main_px,
                "hedge_px": hedge_px,
                "main_q": main_q,
                "hedge_q": hedge_q,
                "seconds_left": base.fnum(row.get("seconds_left")),
                "cost": cost,
            },
        ))
    out.sort(key=lambda r: r[0])
    return out


def summarize(label: str, rows: list[tuple[float, str, float, dict[str, Any]]]) -> dict[str, Any]:
    pnls = [r[2] for r in rows]
    by_day: dict[str, float] = collections.defaultdict(float)
    for end_ts, _slug, pnl, _meta in rows:
        by_day[base.bj_date(end_ts)] += pnl
    costs = [r[3]["cost"] for r in rows]
    ratios = [r[3]["main_q"] / max(r[3]["hedge_q"], 1.0) for r in rows]
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
        "avg_cost": statistics.fmean(costs) if costs else 0.0,
        "max_cost": max(costs) if costs else 0.0,
        "max_ratio": max(ratios) if ratios else 0.0,
        "min_day": min(by_day.values()) if by_day else 0.0,
        "day_pnls": dict(sorted(by_day.items())),
    }


def print_summary(r: dict[str, Any]) -> None:
    days = " ".join(f"{d}:{p:+.0f}" for d, p in r["day_pnls"].items())
    print(
        f"{r['label']:<34s} markets={r['markets']:>4} pnl={r['pnl']:>+9.2f} "
        f"avg={r['avg']:>+7.3f} med={r['median']:>+7.2f} win={100*r['win_rate']:>5.1f}% "
        f"mdd={r['mdd']:>8.2f} worst={r['worst']:>+8.2f} "
        f"cost_avg={r['avg_cost']:>6.1f} cost_max={r['max_cost']:>6.1f} "
        f"maxShareRatio={r['max_ratio']:>4.1f} minDay={r['min_day']:>+7.1f} "
        f"{days}"
    )


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("path")
    ap.add_argument("--universe", choices=["all", "ledger"], default="all")
    ap.add_argument("--min-ticks", type=int, default=1)
    ap.add_argument("--force-seconds", type=int, default=15)
    ap.add_argument("--min-order-usdc", type=float, default=1.0)
    ap.add_argument("--min-hedge-share-frac", type=float, default=0.0)
    ap.add_argument("--times", default="120,90,75,60,50,45,40,35,30,25,20,16")
    ap.add_argument("--modes", default="expensive,z,p,accum_fallback_exp,accum_fallback_p")
    ap.add_argument("--main-shares", default="10,20,30,40,50,75,100,150,200")
    ap.add_argument("--hedge-usdc", default="1,2,3,5")
    ap.add_argument("--top", type=int, default=60)
    ap.add_argument("--show-worst", type=int, default=10)
    args = ap.parse_args()

    data, tmp = base.resolve_data_path(args.path)
    try:
        markets, _state = sim.prepare_markets(data, args.universe, args.min_ticks)
        combos = []
        for tminus, mode, main_shares, hedge_usdc in itertools.product(
            parse_nums(args.times, int),
            [x.strip() for x in args.modes.split(",") if x.strip()],
            parse_nums(args.main_shares, float),
            parse_nums(args.hedge_usdc, float),
        ):
            rows = run_combo(
                markets,
                tminus=tminus,
                mode=mode,
                main_shares=main_shares,
                hedge_usdc=hedge_usdc,
                force_seconds=args.force_seconds,
                min_order_usdc=args.min_order_usdc,
                min_hedge_share_frac=args.min_hedge_share_frac,
            )
            label = f"T-{tminus} {mode} main{main_shares:g} h${hedge_usdc:g}"
            combos.append((summarize(label, rows), rows))

        combos.sort(key=lambda x: (x[0]["pnl"], x[0]["worst"]), reverse=True)
        print(f"[scan] markets={len(markets)} combos={len(combos)} min_order=${args.min_order_usdc:g}")
        print("\n[top pnl]")
        for r, _rows in combos[: args.top]:
            print_summary(r)
        print("\n[top positive with mdd <= 300]")
        safe = [x for x in combos if x[0]["pnl"] > 0 and x[0]["mdd"] <= 300]
        safe.sort(key=lambda x: (x[0]["pnl"], x[0]["worst"]), reverse=True)
        for r, _rows in safe[: args.top]:
            print_summary(r)
        if combos:
            best, best_rows = combos[0]
            print("\n[worst rows for best]")
            print_summary(best)
            for end_ts, slug, pnl, meta in sorted(best_rows, key=lambda r: r[2])[: args.show_worst]:
                print(
                    f"{pnl:+8.2f} {slug} {base.bj_time(end_ts)} "
                    f"{meta['main']}@{meta['main_px']:.3f}x{meta['main_q']:.0f} "
                    f"{meta['hedge']}@{meta['hedge_px']:.3f}x{meta['hedge_q']:.0f} "
                    f"T-{meta['seconds_left']:.0f} cost={meta['cost']:.2f}"
                )
    finally:
        if tmp:
            tmp.cleanup()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
