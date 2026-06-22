#!/usr/bin/env python3
"""Exact-second replay for the accum late-confirm cascade.

This is a taker/FAK research replay:
- merge one or more recorded data directories / .tgz bundles by market slug
- buy a minimum Up+Down base pair in every market
- only trigger cascade rules on exact integer seconds_left values
- add the leading side and then hedge the opposite side to a minimum fraction
"""

from __future__ import annotations

import argparse
import collections
import math
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import analyze_backtest as base
import scan_late_confirm_no_naked as lc
import sim_accum_taker as sim


@dataclass(frozen=True)
class Rule:
    name: str
    seconds_left: int
    bins: tuple[tuple[float, float], ...]
    max_opp_ask: float
    min_spread: float


DEFAULT_RULES: tuple[Rule, ...] = (
    Rule("C200", 200, ((0.79, 0.80),), 0.22, 0.55),
    Rule("C180", 180, ((0.83, 0.85),), 0.25, 0.55),
    Rule("C120", 120, ((0.94, 1.00),), 0.22, 0.70),
    Rule("C60", 60, ((0.85, 0.88),), 0.15, 0.55),
    Rule("C50", 50, ((0.79, 0.81),), 0.22, 0.55),
    Rule("C30", 30, ((0.85, 0.87), (0.87, 0.88), (0.94, 0.96)), 0.20, 0.70),
    Rule("C25", 25, ((0.85, 0.87), (0.88, 0.92)), 0.20, 0.70),
    Rule("C15", 15, ((0.79, 0.82),), 0.25, 0.50),
    Rule("C10", 10, ((0.90, 0.92), (0.96, 0.985)), 0.15, 0.80),
    Rule("C8", 8, ((0.83, 0.89),), 0.18, 0.60),
    Rule("C5", 5, ((0.75, 0.81),), 0.25, 0.50),
)


def exact_tick(ticks: list[dict[str, Any]], seconds_left: int) -> dict[str, Any] | None:
    rows = [
        row
        for row in ticks
        if int(base.fnum(row.get("seconds_left"))) == seconds_left
        and 0.0 < base.fnum(row.get("up_ask")) < 1.0
        and 0.0 < base.fnum(row.get("dn_ask")) < 1.0
    ]
    if not rows:
        return None
    return max(rows, key=lambda row: base.fnum(row.get("seconds_left")))


def entry_tick(
    ticks: list[dict[str, Any]],
    entry_seconds: int,
    force_seconds: int,
) -> dict[str, Any] | None:
    row = exact_tick(ticks, entry_seconds)
    if row is not None:
        return row
    valid = [
        row
        for row in ticks
        if force_seconds < base.fnum(row.get("seconds_left")) <= entry_seconds
        and 0.0 < base.fnum(row.get("up_ask")) < 1.0
        and 0.0 < base.fnum(row.get("dn_ask")) < 1.0
    ]
    if not valid:
        return None
    return max(valid, key=lambda row: base.fnum(row.get("seconds_left")))


def copy_pos(pos: lc.Pos) -> lc.Pos:
    return lc.Pos(
        up_shares=pos.up_shares,
        down_shares=pos.down_shares,
        up_cost=pos.up_cost,
        down_cost=pos.down_cost,
        trades=list(pos.trades),
    )


def price_bin(price: float, bins: tuple[tuple[float, float], ...]) -> int | None:
    for idx, (lo, hi) in enumerate(bins):
        if lo <= price < hi or (idx == len(bins) - 1 and lo <= price <= hi):
            return idx
    return None


def try_rule(
    pos: lc.Pos,
    tick: dict[str, Any],
    winner: str,
    rule: Rule,
    args: argparse.Namespace,
) -> tuple[lc.Pos, dict[str, Any]] | None:
    up_ask = base.fnum(tick.get("up_ask"))
    dn_ask = base.fnum(tick.get("dn_ask"))
    main = "Up" if up_ask >= dn_ask else "Down"
    hedge = "Down" if main == "Up" else "Up"
    main_px = up_ask if main == "Up" else dn_ask
    hedge_px = dn_ask if main == "Up" else up_ask
    spread = main_px - hedge_px
    bin_idx = price_bin(main_px, rule.bins)
    if bin_idx is None or hedge_px > rule.max_opp_ask or spread < rule.min_spread:
        return None

    next_pos = copy_pos(pos)
    target_main = lc.shares_for_request(main_px, args.qty, args.min_order_usdc)
    add_main = max(0.0, target_main - next_pos.shares(main))
    if add_main >= 1.0:
        next_pos.add(main, add_main, main_px, "late_confirm_main")

    target_hedge = math.ceil(next_pos.shares(main) * args.hedge_frac - 1e-9)
    add_hedge = max(0.0, target_hedge - next_pos.shares(hedge))
    if add_hedge >= 1.0:
        next_pos.add(hedge, add_hedge, hedge_px, "late_confirm_hedge")

    if next_pos.cost() > args.max_cost:
        return None

    meta = {
        "confirmed": True,
        "confirm_correct": main == winner,
        "rule": rule.name,
        "bin": bin_idx,
        "main": main,
        "main_px": main_px,
        "hedge_px": hedge_px,
        "seconds_left": rule.seconds_left,
        "target_main": target_main,
    }
    return next_pos, meta


def load_merged_markets(
    paths: list[str],
    universe: str,
    min_ticks: int,
) -> tuple[list[tuple[float, str, str, list[dict[str, Any]]]], list[tempfile.TemporaryDirectory[str]]]:
    tmpdirs: list[tempfile.TemporaryDirectory[str]] = []
    by_slug: dict[str, tuple[float, str, str, list[dict[str, Any]]]] = {}
    for raw in paths:
        data, tmp = base.resolve_data_path(raw)
        if tmp is not None:
            tmpdirs.append(tmp)
        markets, _state = sim.prepare_markets(data, universe, min_ticks)
        for market in markets:
            _end_ts, slug, _winner, ticks = market
            prev = by_slug.get(slug)
            if prev is None or len(ticks) > len(prev[3]):
                by_slug[slug] = market
    return sorted(by_slug.values()), tmpdirs


def run_market(
    end_ts: float,
    slug: str,
    winner: str,
    ticks: list[dict[str, Any]],
    rules: tuple[Rule, ...],
    args: argparse.Namespace,
) -> tuple[float, str, float, lc.Pos, dict[str, Any]] | None:
    entry = entry_tick(ticks, args.entry_seconds, args.force_seconds)
    if entry is None:
        return None

    pos = lc.Pos()
    lc.add_base_pair(
        pos,
        lc.side_price(entry, "Up"),
        lc.side_price(entry, "Down"),
        args.min_order_usdc,
        args.base_usdc,
        args.base_mode,
    )
    meta: dict[str, Any] = {
        "confirmed": False,
        "confirm_correct": None,
        "rule": "",
        "bin": -1,
        "main": "",
        "main_px": 0.0,
        "hedge_px": 0.0,
        "seconds_left": 0,
        "target_main": 0.0,
    }

    for rule in rules:
        tick = exact_tick(ticks, rule.seconds_left)
        if tick is None:
            continue
        applied = try_rule(pos, tick, winner, rule, args)
        if applied is None:
            continue
        pos, meta = applied
        break

    return end_ts, slug, pos.pnl(winner), pos, meta


def summarize(
    rows: list[tuple[float, str, float, lc.Pos, dict[str, Any]]],
    rules: tuple[Rule, ...],
) -> dict[str, Any]:
    report = lc.summarize("exact_accum_cascade", rows)
    confirmed = [row for row in rows if row[4]["confirmed"]]
    by_rule: dict[str, dict[str, float]] = {}
    for rule in rules:
        subset = [row for row in confirmed if row[4]["rule"] == rule.name]
        by_rule[rule.name] = {
            "n": len(subset),
            "pnl": sum(row[2] for row in subset),
            "wrong": sum(1 for row in subset if not row[4]["confirm_correct"]),
        }
    report["by_rule"] = by_rule
    return report


def print_report(
    report: dict[str, Any],
    rows: list[tuple[float, str, float, lc.Pos, dict[str, Any]]],
    args: argparse.Namespace,
) -> None:
    lc.print_summary(report)
    print("[by rule]")
    for name, stats in report["by_rule"].items():
        print(
            f"{name:>4s} n={stats['n']:>3.0f} "
            f"pnl={stats['pnl']:>+9.2f} wrong={stats['wrong']:>3.0f}"
        )
    print("[worst rows]")
    for end_ts, slug, pnl, pos, meta in sorted(rows, key=lambda row: row[2])[: args.show_worst]:
        print(
            f"{pnl:+8.2f} {base.bj_time(end_ts)} {slug} "
            f"up={pos.up_shares:.0f} down={pos.down_shares:.0f} cost={pos.cost():.2f} "
            f"conf={meta['confirmed']} rule={meta['rule']} bin={meta['bin']} "
            f"main={meta['main']} px={meta['main_px']:.3f}/{meta['hedge_px']:.3f} "
            f"T-{meta['seconds_left']}"
        )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("paths", nargs="+")
    parser.add_argument("--universe", choices=["all", "ledger"], default="all")
    parser.add_argument("--min-ticks", type=int, default=1)
    parser.add_argument("--force-seconds", type=int, default=15)
    parser.add_argument("--entry-seconds", type=int, default=300)
    parser.add_argument("--min-order-usdc", type=float, default=1.0)
    parser.add_argument("--base-usdc", type=float, default=1.0)
    parser.add_argument("--base-mode", choices=["usdc", "equal-shares"], default="equal-shares")
    parser.add_argument("--qty", type=float, default=295.0)
    parser.add_argument("--hedge-frac", type=float, default=0.25)
    parser.add_argument("--max-cost", type=float, default=300.0)
    parser.add_argument("--show-worst", type=int, default=12)
    args = parser.parse_args()

    markets, tmpdirs = load_merged_markets(args.paths, args.universe, args.min_ticks)
    try:
        rules = tuple(sorted(DEFAULT_RULES, key=lambda rule: rule.seconds_left, reverse=True))
        rows = []
        skipped = []
        for end_ts, slug, winner, ticks in markets:
            row = run_market(end_ts, slug, winner, ticks, rules, args)
            if row is None:
                skipped.append(slug)
                continue
            rows.append(row)
        rows.sort(key=lambda row: row[0])
        report = summarize(rows, rules)
        print(
            f"[setup] sources={len(args.paths)} markets_loaded={len(markets)} "
            f"markets_traded={len(rows)} skipped={len(skipped)} qty={args.qty:g} "
            f"hedge_frac={args.hedge_frac:g} max_cost={args.max_cost:g} "
            f"base_mode={args.base_mode}"
        )
        if skipped:
            print("[skipped]", " ".join(skipped[:20]))
        print_report(report, rows, args)
    finally:
        for tmp in tmpdirs:
            tmp.cleanup()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
