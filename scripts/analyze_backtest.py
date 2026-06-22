#!/usr/bin/env python3
"""Analyze JY_RUST simulation/live logs before risking real capital.

This is not a market-data replay engine. It audits the bot's own ledger and
signal logs:

  - quant_state.json / quant_state_ideal.json
  - data/quant_signals.jsonl
  - data/books/train_samples.jsonl

Run on the VPS:

  python3 scripts/analyze_backtest.py /opt/jy-data --capital 100 --daily-target 300
"""

from __future__ import annotations

import argparse
import collections
import datetime as dt
import json
import math
import os
from pathlib import Path
import statistics
import tarfile
import tempfile
from typing import Any, Iterable


def bj_date(ts: int | float | None) -> str:
    if not ts:
        return "unknown"
    return dt.datetime.fromtimestamp(float(ts) + 8 * 3600, dt.UTC).strftime("%Y-%m-%d")


def bj_time(ts: int | float | None) -> str:
    if not ts:
        return "unknown"
    return dt.datetime.fromtimestamp(float(ts) + 8 * 3600, dt.UTC).strftime("%Y-%m-%d %H:%M:%S")


def load_env(path: Path) -> dict[str, str]:
    out: dict[str, str] = {}
    if not path.exists():
        return out
    for raw in path.read_text(errors="replace").splitlines():
        line = raw.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        k, v = line.split("=", 1)
        out[k.strip()] = v.strip()
    return out


def resolve_data_path(raw: str) -> tuple[Path, tempfile.TemporaryDirectory[str] | None]:
    """Return a data directory, accepting either a directory or .tgz bundle."""
    p = Path(raw)
    tmp: tempfile.TemporaryDirectory[str] | None = None
    if p.is_file() and p.suffixes[-2:] in [[".tar", ".gz"], [".tgz"]]:
        tmp = tempfile.TemporaryDirectory()
        root = Path(tmp.name)
        with tarfile.open(p, "r:gz") as tf:
            for member in tf.getmembers():
                target = (root / member.name).resolve()
                if not str(target).startswith(str(root.resolve())):
                    raise RuntimeError(f"unsafe tar member path: {member.name}")
            try:
                tf.extractall(root, filter="data")
            except TypeError:
                tf.extractall(root)
        dirs = [x for x in root.iterdir() if x.is_dir()]
        p = dirs[0] if len(dirs) == 1 else root
    if p.is_dir() and not (p / "quant_state.json").exists():
        dirs = [x for x in p.iterdir() if x.is_dir() and (x / "quant_state.json").exists()]
        if len(dirs) == 1:
            p = dirs[0]
    return p, tmp


def iter_jsonl(path: Path) -> Iterable[dict[str, Any]]:
    if not path.exists():
        return
    with path.open(errors="replace") as fh:
        for line_no, line in enumerate(fh, 1):
            line = line.strip()
            if not line:
                continue
            try:
                obj = json.loads(line)
            except json.JSONDecodeError:
                continue
            if isinstance(obj, dict):
                obj["_line"] = line_no
                yield obj


def load_state(path: Path) -> dict[str, dict[str, Any]]:
    if not path.exists():
        return {}
    try:
        obj = json.loads(path.read_text(errors="replace"))
    except json.JSONDecodeError:
        return {}
    if not isinstance(obj, dict):
        return {}
    return {str(k): v for k, v in obj.items() if isinstance(v, dict)}


def fnum(v: Any, default: float = 0.0) -> float:
    try:
        x = float(v)
    except (TypeError, ValueError):
        return default
    return x if math.isfinite(x) else default


def recompute_settle_pnl(pos: dict[str, Any]) -> float | None:
    winner = pos.get("winner")
    if winner not in ("Up", "Down"):
        return None
    up_shares = fnum(pos.get("up_shares"))
    down_shares = fnum(pos.get("down_shares"))
    up_cost = fnum(pos.get("up_cost_total"))
    down_cost = fnum(pos.get("down_cost_total"))
    if winner == "Up":
        return (up_shares - up_cost) - down_cost
    return (down_shares - down_cost) - up_cost


def pct(x: float) -> str:
    return f"{100.0 * x:.1f}%"


def money(x: float) -> str:
    return f"{x:+.2f}u"


def mean(xs: list[float]) -> float:
    return statistics.fmean(xs) if xs else 0.0


def stdev(xs: list[float]) -> float:
    return statistics.stdev(xs) if len(xs) >= 2 else 0.0


def max_drawdown(pnls: list[float]) -> float:
    equity = 0.0
    peak = 0.0
    mdd = 0.0
    for pnl in pnls:
        equity += pnl
        peak = max(peak, equity)
        mdd = max(mdd, peak - equity)
    return mdd


def t_stat(xs: list[float]) -> float:
    if len(xs) < 2:
        return 0.0
    s = stdev(xs)
    if s == 0:
        return 0.0
    return mean(xs) / (s / math.sqrt(len(xs)))


def wilson(success: int, total: int, z: float = 1.96) -> tuple[float, float]:
    if total <= 0:
        return (0.0, 0.0)
    p = success / total
    denom = 1 + z * z / total
    center = (p + z * z / (2 * total)) / denom
    half = z * math.sqrt((p * (1 - p) + z * z / (4 * total)) / total) / denom
    return (max(0.0, center - half), min(1.0, center + half))


def position_exposure(pos: dict[str, Any]) -> float:
    trades = pos.get("trades")
    if not isinstance(trades, list):
        return fnum(pos.get("up_cost_total")) + fnum(pos.get("down_cost_total"))
    return sum(fnum(t.get("total_cost")) for t in trades if isinstance(t, dict))


def taker_fee(price: float, fee_rate: float = 0.07) -> float:
    return fee_rate * price * (1.0 - price)


def full_cost_per_share(price: float, fee_rate: float = 0.07) -> float:
    return price + taker_fee(price, fee_rate)


def analyze_state(name: str, state: dict[str, dict[str, Any]]) -> dict[str, Any]:
    traded = [p for p in state.values() if p.get("trades")]
    settled = [p for p in traded if p.get("realized_pnl") is not None]
    settled.sort(key=lambda p: fnum(p.get("end_ts")))
    pnls = [fnum(p.get("realized_pnl")) for p in settled]
    wins = sum(1 for x in pnls if x >= 0)
    losses = len(pnls) - wins
    start = int(fnum(settled[0].get("end_ts"))) if settled else 0
    end = int(fnum(settled[-1].get("end_ts"))) if settled else 0
    by_day: dict[str, float] = collections.defaultdict(float)
    for p in settled:
        by_day[bj_date(p.get("end_ts"))] += fnum(p.get("realized_pnl"))
    span_days = float(len(by_day)) if by_day else 0.0
    exposures = [position_exposure(p) for p in traded]
    mismatches = 0
    for p in settled:
        rp = recompute_settle_pnl(p)
        if rp is not None and abs(rp - fnum(p.get("realized_pnl"))) > 0.01:
            mismatches += 1
    return {
        "name": name,
        "traded": len(traded),
        "settled": len(settled),
        "open": len(traded) - len(settled),
        "wins": wins,
        "losses": losses,
        "win_rate": wins / len(pnls) if pnls else 0.0,
        "pnl": sum(pnls),
        "avg_pnl": mean(pnls),
        "median_pnl": statistics.median(pnls) if pnls else 0.0,
        "worst": min(pnls) if pnls else 0.0,
        "best": max(pnls) if pnls else 0.0,
        "stdev": stdev(pnls),
        "t_stat": t_stat(pnls),
        "max_drawdown": max_drawdown(pnls),
        "span_days": span_days,
        "avg_daily": sum(pnls) / span_days if span_days else 0.0,
        "by_day": dict(sorted(by_day.items())),
        "max_exposure": max(exposures) if exposures else 0.0,
        "avg_exposure": mean(exposures),
        "settle_mismatches": mismatches,
        "first": bj_time(start) if start else "none",
        "last": bj_time(end) if end else "none",
    }


def settlement_winners(events: list[dict[str, Any]]) -> dict[str, str]:
    out: dict[str, str] = {}
    for e in events:
        if e.get("phase") != "settlement":
            continue
        slug = e.get("slug") or e.get("market")
        winner = e.get("winner")
        if slug and winner in ("Up", "Down"):
            out[str(slug)] = str(winner)
    return out


def joined_accuracy(rows: Iterable[dict[str, Any]], winners: dict[str, str], dir_key: str) -> dict[str, Any]:
    total = 0
    ok = 0
    by_bucket: dict[str, list[int]] = collections.defaultdict(list)
    for r in rows:
        slug = r.get("slug") or r.get("market")
        if slug not in winners:
            continue
        direction = r.get(dir_key)
        if direction not in ("Up", "Down"):
            continue
        hit = 1 if direction == winners[slug] else 0
        total += 1
        ok += hit
        strategy = str(r.get("strategy") or r.get("phase") or "unknown")
        by_bucket[strategy].append(hit)
    lo, hi = wilson(ok, total)
    return {
        "total": total,
        "ok": ok,
        "rate": ok / total if total else 0.0,
        "lo": lo,
        "hi": hi,
        "by_bucket": {k: (sum(v), len(v), sum(v) / len(v)) for k, v in sorted(by_bucket.items()) if v},
    }


def analyze_events(events: list[dict[str, Any]], samples: list[dict[str, Any]]) -> dict[str, Any]:
    winners = settlement_winners(events)
    phase_counts = collections.Counter(str(e.get("phase", "unknown")) for e in events)
    entry_rows = [e for e in events if e.get("phase") == "entry_signal"]
    shadow_rows = [e for e in events if e.get("phase") == "shadow"]

    real_miss = 0
    real_filled = 0
    all_miss = 0
    all_filled = 0
    for e in events:
        phase = e.get("phase")
        if phase == "miss":
            all_miss += 1
            if e.get("dry_run") is False:
                real_miss += 1
        elif phase in {"fill", "ev_solo", "sniper", "accum", "entry", "arb_entry", "zscore"}:
            all_filled += 1
            if e.get("dry_run") is False:
                real_filled += 1

    shadow_bets = [r for r in shadow_rows if r.get("model_bet") is True]
    shadow_bet_acc = joined_accuracy(shadow_bets, winners, "z_dir")
    shadow_all_acc = joined_accuracy(shadow_rows, winners, "z_dir")

    return {
        "winners": winners,
        "phase_counts": phase_counts,
        "entry_accuracy": joined_accuracy(entry_rows, winners, "direction"),
        "sample_accuracy": joined_accuracy(samples, winners, "direction"),
        "shadow_all_accuracy": shadow_all_acc,
        "shadow_bet_accuracy": shadow_bet_acc,
        "all_miss": all_miss,
        "all_filled": all_filled,
        "real_miss": real_miss,
        "real_filled": real_filled,
    }


def default_shares_for(row: dict[str, Any], env: dict[str, str], replay_shares: float | None) -> float:
    if replay_shares and replay_shares > 0:
        return replay_shares
    row_shares = fnum(row.get("shares"))
    if row_shares > 0:
        return row_shares
    strategy = str(row.get("strategy") or row.get("phase") or env.get("ENTRY_STRATEGY", "")).lower()
    keys = []
    if "sniper" in strategy:
        keys.append("SNIPER_QTY")
    if "accum" in strategy:
        keys.append("ACCUM_QTY")
    if "ev_solo" in strategy:
        keys.append("EV_SOLO_QTY")
    keys.extend(["QUANT_ORDER_SHARES", "EV_SOLO_QTY", "SNIPER_QTY", "ACCUM_QTY"])
    for k in keys:
        q = fnum(env.get(k))
        if q > 0:
            return round(q)
    return 1.0


def row_entry_price(row: dict[str, Any]) -> float:
    for key in ["entry_ask", "ask", "price", "filled_price"]:
        p = fnum(row.get(key))
        if 0.0 < p < 1.0:
            return p
    return 0.0


def replay_rows(
    label: str,
    rows: Iterable[dict[str, Any]],
    winners: dict[str, str],
    env: dict[str, str],
    replay_shares: float | None,
    fee_rate: float,
) -> dict[str, Any]:
    pnls: list[float] = []
    used_rows: list[dict[str, Any]] = []
    skipped = 0
    by_day: dict[str, float] = collections.defaultdict(float)
    by_bucket: dict[str, list[float]] = collections.defaultdict(list)
    for row in rows:
        slug = row.get("slug") or row.get("market")
        winner = winners.get(str(slug)) if slug is not None else None
        direction = row.get("direction") or row.get("z_dir")
        price = row_entry_price(row)
        shares = default_shares_for(row, env, replay_shares)
        if winner not in ("Up", "Down") or direction not in ("Up", "Down") or price <= 0 or shares <= 0:
            skipped += 1
            continue
        fc = full_cost_per_share(price, fee_rate)
        pnl = shares * ((1.0 - fc) if direction == winner else -fc)
        pnls.append(pnl)
        used_rows.append(row)
        by_day[bj_date(row.get("ts") or row.get("end_ts"))] += pnl
        bucket = str(row.get("strategy") or row.get("phase") or label)
        by_bucket[bucket].append(pnl)
    wins = sum(1 for x in pnls if x >= 0)
    return {
        "label": label,
        "rows": len(used_rows),
        "skipped": skipped,
        "wins": wins,
        "losses": len(pnls) - wins,
        "win_rate": wins / len(pnls) if pnls else 0.0,
        "pnl": sum(pnls),
        "avg_pnl": mean(pnls),
        "median_pnl": statistics.median(pnls) if pnls else 0.0,
        "worst": min(pnls) if pnls else 0.0,
        "best": max(pnls) if pnls else 0.0,
        "stdev": stdev(pnls),
        "t_stat": t_stat(pnls),
        "max_drawdown": max_drawdown(pnls),
        "days": len(by_day),
        "avg_daily": sum(pnls) / len(by_day) if by_day else 0.0,
        "by_day": dict(sorted(by_day.items())),
        "by_bucket": {k: {
            "rows": len(v),
            "pnl": sum(v),
            "avg_pnl": mean(v),
            "win_rate": sum(1 for x in v if x >= 0) / len(v),
        } for k, v in sorted(by_bucket.items()) if v},
    }


def replay_signal_backtests(
    events: list[dict[str, Any]],
    samples: list[dict[str, Any]],
    winners: dict[str, str],
    env: dict[str, str],
    replay_shares: float | None,
    fee_rate: float,
) -> list[dict[str, Any]]:
    entry_rows = [e for e in events if e.get("phase") == "entry_signal"]
    sniper_rows = [e for e in events if e.get("phase") == "sniper_entry"]
    actual_strategy_phases = {
        "entry", "ev_solo", "sniper",
        "accum_first", "accum_chase", "accum_trend", "accum_dip", "accum_rescue",
        "zquote_dir", "zquote_opp",
    }
    actual_strategy_rows = [
        e for e in events
        if e.get("phase") in actual_strategy_phases and row_entry_price(e) > 0
    ]
    train_rows = [s for s in samples if s.get("kind") == "train_sample"]
    reports = [
        replay_rows("entry_signal", entry_rows, winners, env, replay_shares, fee_rate),
        replay_rows("sniper_entry", sniper_rows, winners, env, replay_shares, fee_rate),
        replay_rows("actual_strategy_rows", actual_strategy_rows, winners, env, replay_shares, fee_rate),
        replay_rows("train_samples", train_rows, winners, env, replay_shares, fee_rate),
    ]
    return [r for r in reports if r["rows"] or r["skipped"]]


def sample_lookup(samples: list[dict[str, Any]]) -> dict[str, dict[str, Any]]:
    out: dict[str, dict[str, Any]] = {}
    for sample in samples:
        if sample.get("kind") != "train_sample":
            continue
        slug = sample.get("slug") or sample.get("market")
        if slug is not None and str(slug) not in out:
            out[str(slug)] = sample
    return out


def accum_first_joined_rows(events: list[dict[str, Any]], samples: list[dict[str, Any]]) -> list[dict[str, Any]]:
    by_slug = sample_lookup(samples)
    rows: list[dict[str, Any]] = []
    for event in events:
        if event.get("phase") != "accum_first":
            continue
        slug = event.get("market") or event.get("slug")
        if slug is None:
            continue
        sample = by_slug.get(str(slug), {})
        if not sample and event.get("z") is None:
            continue
        row = dict(event)
        row["slug"] = str(slug)
        row["z"] = fnum(event.get("z"), fnum(sample.get("z")))
        row["sample_entry_ask"] = fnum(sample.get("entry_ask"))
        row["bj_hour"] = sample.get("bj_hour")
        row["ask_sum"] = fnum(sample.get("ask_sum"))
        row["seconds_left"] = fnum(event.get("seconds_left"), fnum(sample.get("seconds_left")))
        row["flow_imb_60"] = fnum(event.get("flow_imb_60"), fnum(sample.get("flow_imb_60")))
        direction = row.get("direction") or sample.get("direction")
        sign = 1.0 if direction == "Up" else -1.0 if direction == "Down" else 0.0
        row["dir_flow_60"] = fnum(event.get("dir_flow_60"), sign * row["flow_imb_60"])
        rows.append(row)
    rows.sort(key=lambda r: fnum(r.get("ts")))
    return rows


def replay_split_report(
    label: str,
    rows: list[dict[str, Any]],
    winners: dict[str, str],
    env: dict[str, str],
    replay_shares: float | None,
    fee_rate: float,
) -> dict[str, Any]:
    report = replay_rows(label, rows, winners, env, replay_shares, fee_rate)
    mid = len(rows) // 2
    first = replay_rows(f"{label}_first_half", rows[:mid], winners, env, replay_shares, fee_rate)
    second = replay_rows(f"{label}_second_half", rows[mid:], winners, env, replay_shares, fee_rate)
    report["first_half_avg"] = first["avg_pnl"]
    report["second_half_avg"] = second["avg_pnl"]
    report["first_half_pnl"] = first["pnl"]
    report["second_half_pnl"] = second["pnl"]
    return report


def accum_filter_scan(
    events: list[dict[str, Any]],
    samples: list[dict[str, Any]],
    winners: dict[str, str],
    env: dict[str, str],
    replay_shares: float | None,
    fee_rate: float,
) -> dict[str, Any]:
    rows = accum_first_joined_rows(events, samples)
    entry_z = fnum(env.get("ACCUM_ENTRY_Z"), 0.15)
    max_ask = fnum(env.get("ACCUM_ENTRY_MAX_ASK"), 1.0)
    max_dir_flow_60 = fnum(env.get("ACCUM_ENTRY_MAX_DIR_FLOW_60"), 1.0)

    def pick(z_min: float, ask_max: float, dir_flow_max: float = 1.0) -> list[dict[str, Any]]:
        return [
            row for row in rows
            if abs(fnum(row.get("z"))) >= z_min and row_entry_price(row) <= ask_max
            and fnum(row.get("dir_flow_60")) <= dir_flow_max
        ]

    grid: list[dict[str, Any]] = []
    for z_min in [0.3, 0.5, 0.7, 1.0]:
        for ask_max in [0.52, 0.55, 0.60, 1.0]:
            label = f"absz>={z_min:.2f} ask<={ask_max:.2f}"
            subset = pick(z_min, ask_max)
            report = replay_split_report(label, subset, winners, env, replay_shares, fee_rate)
            report["z_min"] = z_min
            report["ask_max"] = ask_max
            grid.append(report)

    flow_grid: list[dict[str, Any]] = []
    for ask_max in [0.55, 0.60]:
        for flow_max in [0.0, 0.12, 0.24, 0.36, 1.0]:
            label = f"absz>=0.50 ask<={ask_max:.2f} dir_flow_60<={flow_max:.2f}"
            report = replay_split_report(
                label,
                pick(0.50, ask_max, flow_max),
                winners,
                env,
                replay_shares,
                fee_rate,
            )
            report["ask_max"] = ask_max
            report["flow_max"] = flow_max
            flow_grid.append(report)

    current = replay_split_report(
        f"current absz>={entry_z:.2f} ask<={max_ask:.2f} dir_flow_60<={max_dir_flow_60:.2f}",
        pick(entry_z, max_ask, max_dir_flow_60),
        winners,
        env,
        replay_shares,
        fee_rate,
    )
    baseline = replay_split_report("all accum_first joined", rows, winners, env, replay_shares, fee_rate)
    return {"rows": rows, "baseline": baseline, "current": current, "grid": grid, "flow_grid": flow_grid}


def print_state_report(summary: dict[str, Any], capital: float, daily_target: float) -> None:
    n = summary["settled"]
    print(f"\n[{summary['name']}] ledger")
    print(f"  range: {summary['first']} -> {summary['last']}")
    print(f"  trading days: {summary['span_days']:.0f}")
    print(f"  traded/settled/open: {summary['traded']} / {summary['settled']} / {summary['open']}")
    print(f"  win/loss: {summary['wins']} / {summary['losses']} ({pct(summary['win_rate'])})")
    print(f"  net pnl: {money(summary['pnl'])}, avg/trade: {money(summary['avg_pnl'])}, median: {money(summary['median_pnl'])}")
    print(f"  best/worst: {money(summary['best'])} / {money(summary['worst'])}, stdev: {summary['stdev']:.2f}, t-stat: {summary['t_stat']:.2f}")
    print(f"  max drawdown: {summary['max_drawdown']:.2f}u ({pct(summary['max_drawdown'] / capital) if capital else 'n/a'} of {capital:.0f}u)")
    print(f"  exposure avg/max: {summary['avg_exposure']:.2f}u / {summary['max_exposure']:.2f}u")
    print(f"  avg daily pnl: {money(summary['avg_daily'])} vs target {daily_target:.2f}u/day")
    if summary["settle_mismatches"]:
        print(f"  warning: {summary['settle_mismatches']} settled rows do not match recomputed fee-aware PnL")
    if summary["by_day"]:
        print("  daily pnl:")
        for day, pnl in summary["by_day"].items():
            print(f"    {day}: {money(pnl)}")
    if n:
        needed = daily_target / max(abs(summary["avg_daily"]), 1e-9)
        if summary["avg_daily"] > 0:
            print(f"  target multiple needed from current avg daily: {needed:.1f}x")
        else:
            print("  target multiple needed from current avg daily: impossible while avg daily <= 0")


def print_accuracy(label: str, acc: dict[str, Any]) -> None:
    print(f"\n[{label}] direction accuracy")
    print(f"  joined samples: {acc['total']}, correct: {acc['ok']}, rate: {pct(acc['rate'])}")
    if acc["total"]:
        print(f"  95% Wilson interval: {pct(acc['lo'])} - {pct(acc['hi'])}")
    if acc["by_bucket"]:
        print("  by bucket:")
        for k, (ok, total, rate) in acc["by_bucket"].items():
            print(f"    {k}: {ok}/{total} ({pct(rate)})")


def print_replay_report(report: dict[str, Any], capital: float, daily_target: float) -> None:
    print(f"\n[signal replay: {report['label']}]")
    print(f"  rows/skipped: {report['rows']} / {report['skipped']}")
    if not report["rows"]:
        return
    print(f"  win/loss: {report['wins']} / {report['losses']} ({pct(report['win_rate'])})")
    print(f"  net pnl: {money(report['pnl'])}, avg/row: {money(report['avg_pnl'])}, median: {money(report['median_pnl'])}")
    print(f"  best/worst: {money(report['best'])} / {money(report['worst'])}, t-stat: {report['t_stat']:.2f}")
    print(f"  max drawdown: {report['max_drawdown']:.2f}u ({pct(report['max_drawdown'] / capital) if capital else 'n/a'} of {capital:.0f}u)")
    print(f"  trading days: {report['days']}, avg daily pnl: {money(report['avg_daily'])} vs target {daily_target:.2f}u/day")
    if report["by_bucket"]:
        print("  by bucket:")
        for k, v in report["by_bucket"].items():
            print(f"    {k}: rows={v['rows']} pnl={money(v['pnl'])} avg={money(v['avg_pnl'])} win={pct(v['win_rate'])}")


def print_accum_filter_scan(scan: dict[str, Any], capital: float) -> None:
    rows = scan["rows"]
    print("\n[accum+fak first-entry filter scan]")
    print(f"  joined accum_first rows: {len(rows)}")
    if not rows:
        return
    print("  scope: first accum entry only; chase/dip/rescue legs excluded")
    for label in ["baseline", "current"]:
        report = scan[label]
        print(
            f"  {label}: rows={report['rows']} pnl={money(report['pnl'])} "
            f"avg={money(report['avg_pnl'])} win={pct(report['win_rate'])} "
            f"mdd={report['max_drawdown']:.2f}u ({pct(report['max_drawdown'] / capital) if capital else 'n/a'}) "
            f"split_avg={money(report['first_half_avg'])}/{money(report['second_half_avg'])}"
        )
    print("  selected grid:")
    for report in scan["grid"]:
        if report["rows"] == 0:
            continue
        print(
            f"    z>={report['z_min']:.2f} ask<={report['ask_max']:.2f}: "
            f"rows={report['rows']} pnl={money(report['pnl'])} "
            f"avg={money(report['avg_pnl'])} win={pct(report['win_rate'])} "
            f"split={money(report['first_half_avg'])}/{money(report['second_half_avg'])}"
        )
    print("  flow grid:")
    for report in scan["flow_grid"]:
        if report["rows"] == 0:
            continue
        print(
            f"    ask<={report['ask_max']:.2f} dir_flow_60<={report['flow_max']:.2f}: "
            f"rows={report['rows']} pnl={money(report['pnl'])} "
            f"avg={money(report['avg_pnl'])} win={pct(report['win_rate'])} "
            f"mdd={report['max_drawdown']:.2f} split={money(report['first_half_avg'])}/{money(report['second_half_avg'])}"
        )


def print_gates(real: dict[str, Any] | None, event_summary: dict[str, Any], capital: float, daily_target: float) -> None:
    print("\n[risk gates]")
    if not real or real["settled"] == 0:
        print("  FAIL no settled ledger rows; cannot justify live trading")
        return
    if real["settled"] < 100:
        print(f"  WARN only {real['settled']} settled markets; too small for high-confidence edge")
    else:
        print(f"  PASS sample size >= 100 settled markets ({real['settled']})")
    if real["avg_daily"] <= 0:
        print(f"  FAIL avg daily pnl is not positive ({money(real['avg_daily'])})")
    elif real["avg_daily"] < daily_target:
        print(f"  FAIL avg daily pnl {money(real['avg_daily'])} is below requested {daily_target:.2f}u/day")
    else:
        print(f"  PASS avg daily pnl meets requested target ({money(real['avg_daily'])})")
    if real["max_drawdown"] > capital * 0.25:
        print(f"  FAIL max drawdown {real['max_drawdown']:.2f}u exceeds 25% of {capital:.0f}u capital")
    else:
        print(f"  PASS max drawdown within 25% capital gate")
    if real["max_exposure"] > capital * 0.25:
        print(f"  WARN max single-market exposure {real['max_exposure']:.2f}u exceeds 25% of capital")
    else:
        print("  PASS max single-market exposure within 25% capital gate")
    if real["t_stat"] < 2.0:
        print(f"  WARN PnL t-stat {real['t_stat']:.2f} is weak; edge may be noise")
    else:
        print(f"  PASS PnL t-stat >= 2 ({real['t_stat']:.2f})")

    real_orders = event_summary["real_miss"] + event_summary["real_filled"]
    if real_orders:
        miss_rate = event_summary["real_miss"] / real_orders
        if miss_rate > 0.25:
            print(f"  WARN live miss rate is high: {event_summary['real_miss']}/{real_orders} ({pct(miss_rate)})")
        else:
            print(f"  PASS live miss rate acceptable: {event_summary['real_miss']}/{real_orders} ({pct(miss_rate)})")


def recommendation(real: dict[str, Any] | None, event_summary: dict[str, Any], capital: float, daily_target: float) -> tuple[str, list[str]]:
    reasons: list[str] = []
    if not real or real["settled"] == 0:
        return "NO_DATA", ["no settled ledger rows were found"]
    if real["settle_mismatches"]:
        reasons.append("ledger PnL does not fully match fee-aware recomputation")
    if real["settled"] < 100:
        reasons.append(f"sample is small: {real['settled']} settled markets")
    if real["avg_daily"] <= 0:
        reasons.append(f"average daily PnL is not positive: {money(real['avg_daily'])}")
        return "PAPER_ONLY", reasons
    if real["max_drawdown"] > capital * 0.25:
        reasons.append(f"drawdown exceeds 25% capital gate: {real['max_drawdown']:.2f}u")
    if real["max_exposure"] > capital * 0.25:
        reasons.append(f"single-market exposure exceeds 25% capital gate: {real['max_exposure']:.2f}u")
    if real["t_stat"] < 2.0:
        reasons.append(f"PnL t-stat is weak: {real['t_stat']:.2f}")
    real_orders = event_summary["real_miss"] + event_summary["real_filled"]
    if real_orders and event_summary["real_miss"] / real_orders > 0.25:
        reasons.append(f"live miss rate is high: {pct(event_summary['real_miss'] / real_orders)}")
    if real["avg_daily"] < daily_target:
        reasons.append(f"average daily PnL {money(real['avg_daily'])} is below requested {daily_target:.2f}u/day")
        return "DOES_NOT_MEET_TARGET", reasons
    if reasons:
        return "TARGET_HIT_BUT_TOO_RISKY", reasons
    return "MICRO_LIVE_CANDIDATE", ["all conservative gates passed; still start with 1 share and a hard stop"]


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("data_dir", nargs="?", default="/opt/jy-data", help="JY data directory or jy-backtest-bundle-*.tgz")
    ap.add_argument("--capital", type=float, default=100.0)
    ap.add_argument("--daily-target", type=float, default=300.0)
    ap.add_argument("--replay-shares", type=float, default=None, help="Override per-signal shares for signal replay, e.g. 1 for micro-live sizing")
    ap.add_argument("--fee-rate", type=float, default=None, help="Override taker fee rate; defaults to TAKER_FEE_RATE or 0.07")
    args = ap.parse_args()

    data, extracted = resolve_data_path(args.data_dir)
    env = load_env(data / ".env")
    if not env:
        env = load_env(data / "config_redacted.txt")
    state_path = data / env.get("QUANT_STATE_FILE", "quant_state.json")
    if not state_path.is_absolute():
        state_path = data / state_path
    signals_path = data / env.get("QUANT_SIGNAL_FILE", "data/quant_signals.jsonl")
    if not signals_path.is_absolute():
        signals_path = data / signals_path
    books_dir = data / env.get("BOOK_RECORD_DIR", "data/books")
    if not books_dir.is_absolute():
        books_dir = data / books_dir

    ideal_path = state_path.with_name(f"{state_path.stem}_ideal{state_path.suffix}")
    samples_path = books_dir / "train_samples.jsonl"

    print("[config]")
    print(f"  data_dir: {data}")
    print(f"  state: {state_path} ({'ok' if state_path.exists() else 'missing'})")
    print(f"  ideal_state: {ideal_path} ({'ok' if ideal_path.exists() else 'missing'})")
    print(f"  signals: {signals_path} ({'ok' if signals_path.exists() else 'missing'})")
    print(f"  train_samples: {samples_path} ({'ok' if samples_path.exists() else 'missing'})")
    for k in [
        "DRY_RUN", "ENTRY_STRATEGY", "ORDER_MODE",
        "EV_SOLO_QTY", "SNIPER_QTY", "ACCUM_QTY", "QUANT_ORDER_SHARES",
        "ACCUM_ENTRY_Z", "ACCUM_ENTRY_MAX_ASK", "ACCUM_ENTRY_MAX_DIR_FLOW_60",
        "ACCUM_ENTRY_MIN_SECONDS_LEFT", "ACCUM_CHASE_LEVELS",
        "ACCUM_DIP_LEVELS", "ACCUM_RESCUE_SECS",
    ]:
        if k in env:
            print(f"  {k}={env[k]}")

    real_state = load_state(state_path)
    ideal_state = load_state(ideal_path)
    events = list(iter_jsonl(signals_path))
    samples = list(iter_jsonl(samples_path))
    fee_rate = args.fee_rate if args.fee_rate is not None else fnum(env.get("TAKER_FEE_RATE"), 0.07)

    real_summary = analyze_state("real/sim ledger", real_state) if real_state else None
    ideal_summary = analyze_state("ideal ledger", ideal_state) if ideal_state else None
    event_summary = analyze_events(events, samples)
    replay_reports = replay_signal_backtests(
        events, samples, event_summary["winners"], env, args.replay_shares, fee_rate
    )
    accum_scan = accum_filter_scan(
        events, samples, event_summary["winners"], env, args.replay_shares, fee_rate
    )

    if real_summary:
        print_state_report(real_summary, args.capital, args.daily_target)
    else:
        print("\n[real/sim ledger] missing or empty")

    if ideal_summary:
        print_state_report(ideal_summary, args.capital, args.daily_target)
        if real_summary and real_summary["settled"] and ideal_summary["settled"]:
            gap = real_summary["pnl"] - ideal_summary["pnl"]
            print(f"\n[real vs ideal]")
            print(f"  real - ideal pnl gap: {money(gap)}")

    print("\n[signals]")
    print(f"  jsonl events: {len(events)}, settlements: {len(event_summary['winners'])}")
    if event_summary["phase_counts"]:
        for phase, n in event_summary["phase_counts"].most_common(12):
            print(f"  phase {phase}: {n}")
    all_orders = event_summary["all_miss"] + event_summary["all_filled"]
    if all_orders:
        print(f"  all-order miss rate: {event_summary['all_miss']}/{all_orders} ({pct(event_summary['all_miss'] / all_orders)})")
    real_orders = event_summary["real_miss"] + event_summary["real_filled"]
    if real_orders:
        print(f"  live-order miss rate: {event_summary['real_miss']}/{real_orders} ({pct(event_summary['real_miss'] / real_orders)})")

    print_accuracy("entry_signal", event_summary["entry_accuracy"])
    print_accuracy("train_samples", event_summary["sample_accuracy"])
    print_accuracy("shadow_all", event_summary["shadow_all_accuracy"])
    print_accuracy("shadow_model_bets", event_summary["shadow_bet_accuracy"])

    print("\n[signal replay assumptions]")
    print(f"  fee_rate: {fee_rate:.4f}")
    print(f"  replay_shares: {'env/default per row' if args.replay_shares is None else args.replay_shares}")
    print("  formula: win = shares*(1 - (price + fee)), loss = -shares*(price + fee)")
    for report in replay_reports:
        print_replay_report(report, args.capital, args.daily_target)

    print_accum_filter_scan(accum_scan, args.capital)

    print_gates(real_summary, event_summary, args.capital, args.daily_target)

    verdict, reasons = recommendation(real_summary, event_summary, args.capital, args.daily_target)
    print("\n[verdict]")
    print(f"  {verdict}")
    for reason in reasons:
        print(f"  - {reason}")

    print("\n[target math]")
    print(f"  requested daily target: {args.daily_target:.2f}u/day on {args.capital:.0f}u capital = {pct(args.daily_target / args.capital)} per day")
    if real_summary and real_summary["span_days"] > 0:
        trades_per_day = real_summary["settled"] / real_summary["span_days"]
        needed_avg_trade = args.daily_target / trades_per_day if trades_per_day > 0 else 0.0
        print(f"  observed settled markets/day: {trades_per_day:.1f}")
        print(f"  required avg PnL/trade for target at observed frequency: {money(needed_avg_trade)}")
        print(f"  observed avg PnL/trade: {money(real_summary['avg_pnl'])}")
    print("  100u -> 10000w u means roughly 1,000,000x; treat that as fantasy target, not a risk plan")
    if extracted:
        extracted.cleanup()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
