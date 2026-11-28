"""Budget parser, comparator and ratchet check for the perf gate (#921).

Pure functions, standard library only (Python >= 3.11 for tomllib), no
measurement here: `run.py` measures, this module decides.

budget.toml shape (see that file for the rationale per metric):

    [x86_64-linux.cold_start_ms]
    baseline = 8.0      # what run 1 measured
    ceiling  = 25.0     # the gate: measured > ceiling fails
"""

from __future__ import annotations

import math
import tomllib
from dataclasses import dataclass

METRICS = ("cold_start_ms", "idle_rss_mb", "active_rss_mb", "per_turn_ms", "binary_size_mb")


class BudgetError(ValueError):
    """The budget file or a report is malformed."""


def parse_budget(text: str) -> dict[str, dict[str, float]]:
    """Return {platform: {metric: ceiling}}. Strict: unknown metric, a
    missing ceiling, a non-positive or non-numeric value is an error."""
    try:
        doc = tomllib.loads(text)
    except tomllib.TOMLDecodeError as e:
        raise BudgetError(f"budget.toml is not valid TOML: {e}") from e
    out: dict[str, dict[str, float]] = {}
    for platform, table in doc.items():
        if not isinstance(table, dict):
            raise BudgetError(f"[{platform}] must be a table of metrics")
        metrics: dict[str, float] = {}
        for name, spec in table.items():
            if name not in METRICS:
                raise BudgetError(f"[{platform}.{name}]: unknown metric (known: {', '.join(METRICS)})")
            if not isinstance(spec, dict) or "ceiling" not in spec:
                raise BudgetError(f"[{platform}.{name}] needs a `ceiling`")
            ceiling = spec["ceiling"]
            if isinstance(ceiling, bool) or not isinstance(ceiling, (int, float)) or ceiling <= 0:
                raise BudgetError(f"[{platform}.{name}] ceiling must be a positive number, got {ceiling!r}")
            metrics[name] = float(ceiling)
        missing = [m for m in METRICS if m not in metrics]
        if missing:
            raise BudgetError(f"[{platform}] is missing metric(s): {', '.join(missing)}")
        out[platform] = metrics
    return out


@dataclass(frozen=True)
class Verdict:
    metric: str
    measured: float
    ceiling: float

    @property
    def ok(self) -> bool:
        return self.measured <= self.ceiling

    @property
    def headroom_pct(self) -> float:
        return (self.ceiling - self.measured) / self.ceiling * 100.0


def compare(measured: dict[str, float], ceilings: dict[str, float]) -> list[Verdict]:
    """One verdict per budgeted metric. A budgeted metric that was not
    measured is an error: a gate that silently skips is no gate."""
    verdicts = []
    for metric, ceiling in ceilings.items():
        if metric not in measured:
            raise BudgetError(f"metric {metric} is budgeted but the report does not carry it")
        value = measured[metric]
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            raise BudgetError(f"metric {metric}: measured value {value!r} is not a number")
        if not math.isfinite(value) or value < 0:
            raise BudgetError(f"metric {metric}: measured value {value!r} is not a finite, non-negative number")
        verdicts.append(Verdict(metric, float(value), ceiling))
    return verdicts


def ratchet_violations(
    base: dict[str, dict[str, float]], head: dict[str, dict[str, float]]
) -> list[str]:
    """Ceilings may only move down. Returns one message per ceiling that
    is higher on `head` than on `base`. A platform or metric that is new on
    head is not a violation (a first budget has nothing to ratchet from);
    one that disappeared is, because dropping a ceiling silently un-gates it."""
    problems = []
    for platform, base_metrics in base.items():
        head_metrics = head.get(platform)
        if head_metrics is None:
            problems.append(f"{platform}: budget removed")
            continue
        for metric, old in base_metrics.items():
            new = head_metrics.get(metric)
            if new is None:
                problems.append(f"{platform}.{metric}: budget removed")
            elif new > old:
                problems.append(f"{platform}.{metric}: ceiling raised {old:g} -> {new:g}")
    return problems
