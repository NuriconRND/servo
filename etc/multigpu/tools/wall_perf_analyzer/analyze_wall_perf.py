#!/usr/bin/env python3
"""Summarize Servo wall-layout performance diagnostics.

The analyzer intentionally depends only on Python's standard library so it can
run on the Windows development machine without adding another project build.
"""

from __future__ import annotations

import argparse
import json
import math
import re
from collections import Counter, defaultdict
from dataclasses import dataclass, field
from pathlib import Path
from statistics import mean
from typing import Iterable


RENDER_RE = re.compile(
    r"Wall render end: painter (?P<painter>PainterId\(\d+\)).*?"
    r"logical_frame_id=(?P<logical>None|Some\((?P<logical_id>\d+)\)).*?"
    r"render_ms=(?P<render_ms>[0-9.]+).*?"
    r"requested_gpu=(?P<gpu>None|Some\((?P<gpu_id>\d+)\))"
)
REPAINT_RE = re.compile(
    r"Wall repaint target: .*? target=(?P<target>\w+).*?"
    r"requested_gpu=(?P<gpu>None|Some\((?P<gpu_id>\d+)\)).*?"
    r"render_ms=(?P<render_ms>[0-9.]+).*?"
    r"target_present_ms=(?P<target_present_ms>[0-9.]+)"
)
PRESENT_RE = re.compile(
    r"Wall window present: .*? tile=(?P<tile>\d+) monitor=(?P<monitor>\d+) "
    r"gpu=(?P<gpu>\d+) requested_gpu=(?P<requested_gpu>None|Some\((?P<requested_gpu_id>\d+)\)) "
    r"present_ms=(?P<present_ms>[0-9.]+)"
)
BARRIER_COMPLETE_RE = re.compile(
    r"Wall frame barrier complete: logical_frame_id=(?P<logical>\d+) "
    r"status=(?P<status>\w+) ready=(?P<ready>\d+)/(?P<expected>\d+) "
    r"first_to_all_ready_ms=(?P<first_to_all_ready_ms>[0-9.]+) "
    r"request_to_all_ready_ms=(?P<request_to_all_ready_ms>[0-9.]+).*?"
    r"final_wait_ms=(?P<final_wait_ms>[0-9.]+) need_repaint=(?P<need_repaint>\w+)"
)
BARRIER_MISSED_RE = re.compile(
    r"Wall frame barrier missed: logical_frame_id=(?P<logical>\d+) "
    r"ready=(?P<ready>\d+)/(?P<expected>\d+).*?"
    r"first_ready_elapsed_ms=(?P<first_ready_elapsed_ms>[0-9.]+).*?"
    r"need_repaint=(?P<need_repaint>\w+)"
)
LOGICAL_FRAME_RE = re.compile(r"Wall logical frame (?P<logical>\d+) fan-out")
METADATA_MATCH_RE = re.compile(r"Wall frame metadata: .*?scroll_offsets=matched")
METADATA_MISMATCH_RE = re.compile(r"Wall frame metadata mismatch")


@dataclass
class SeriesSummary:
    count: int = 0
    minimum: float | None = None
    average: float | None = None
    p50: float | None = None
    p95: float | None = None
    p99: float | None = None
    maximum: float | None = None

    @classmethod
    def from_values(cls, values: Iterable[float]) -> "SeriesSummary":
        sorted_values = sorted(values)
        if not sorted_values:
            return cls()

        return cls(
            count=len(sorted_values),
            minimum=sorted_values[0],
            average=mean(sorted_values),
            p50=percentile(sorted_values, 0.50),
            p95=percentile(sorted_values, 0.95),
            p99=percentile(sorted_values, 0.99),
            maximum=sorted_values[-1],
        )

    def to_dict(self) -> dict[str, float | int | None]:
        return {
            "count": self.count,
            "min_ms": round_optional(self.minimum),
            "avg_ms": round_optional(self.average),
            "p50_ms": round_optional(self.p50),
            "p95_ms": round_optional(self.p95),
            "p99_ms": round_optional(self.p99),
            "max_ms": round_optional(self.maximum),
        }


@dataclass
class LogSummary:
    path: str
    logical_frames: int = 0
    metadata_matched: int = 0
    metadata_mismatched: int = 0
    barrier_completed: int = 0
    barrier_missed: int = 0
    skipped_repaint_targets: int = 0
    panics: int = 0
    errors: int = 0
    missed_frame_logs: int = 0
    pending_frame_warnings: int = 0
    unexpected_ready_logs: int = 0
    render_ms: list[float] = field(default_factory=list)
    repaint_render_ms: list[float] = field(default_factory=list)
    target_present_ms: list[float] = field(default_factory=list)
    window_present_ms: list[float] = field(default_factory=list)
    first_to_all_ready_ms: list[float] = field(default_factory=list)
    request_to_all_ready_ms: list[float] = field(default_factory=list)
    final_wait_ms: list[float] = field(default_factory=list)
    missed_first_ready_elapsed_ms: list[float] = field(default_factory=list)
    render_ms_by_painter: dict[str, list[float]] = field(default_factory=lambda: defaultdict(list))
    window_present_ms_by_tile: dict[str, list[float]] = field(
        default_factory=lambda: defaultdict(list)
    )
    requested_gpu_counts: Counter[str] = field(default_factory=Counter)
    tile_gpu_counts: Counter[str] = field(default_factory=Counter)

    def to_dict(self) -> dict[str, object]:
        return {
            "path": self.path,
            "counts": {
                "logical_frames": self.logical_frames,
                "metadata_matched": self.metadata_matched,
                "metadata_mismatched": self.metadata_mismatched,
                "barrier_completed": self.barrier_completed,
                "barrier_missed": self.barrier_missed,
                "skipped_repaint_targets": self.skipped_repaint_targets,
                "panics": self.panics,
                "errors": self.errors,
                "missed_frame_logs": self.missed_frame_logs,
                "pending_frame_warnings": self.pending_frame_warnings,
                "unexpected_ready_logs": self.unexpected_ready_logs,
            },
            "series": {
                "painter_render": SeriesSummary.from_values(self.render_ms).to_dict(),
                "repaint_render": SeriesSummary.from_values(self.repaint_render_ms).to_dict(),
                "target_present": SeriesSummary.from_values(self.target_present_ms).to_dict(),
                "window_present": SeriesSummary.from_values(self.window_present_ms).to_dict(),
                "barrier_first_to_all_ready": SeriesSummary.from_values(
                    self.first_to_all_ready_ms
                ).to_dict(),
                "barrier_request_to_all_ready": SeriesSummary.from_values(
                    self.request_to_all_ready_ms
                ).to_dict(),
                "barrier_final_wait": SeriesSummary.from_values(self.final_wait_ms).to_dict(),
                "missed_barrier_first_ready_elapsed": SeriesSummary.from_values(
                    self.missed_first_ready_elapsed_ms
                ).to_dict(),
            },
            "by_painter": {
                painter: SeriesSummary.from_values(values).to_dict()
                for painter, values in sorted(self.render_ms_by_painter.items())
            },
            "by_tile": {
                tile: SeriesSummary.from_values(values).to_dict()
                for tile, values in sorted(self.window_present_ms_by_tile.items())
            },
            "requested_gpu_counts": dict(sorted(self.requested_gpu_counts.items())),
            "tile_gpu_counts": dict(sorted(self.tile_gpu_counts.items())),
            "present_balance": present_balance(self.window_present_ms_by_tile),
        }


def percentile(sorted_values: list[float], quantile: float) -> float:
    if len(sorted_values) == 1:
        return sorted_values[0]

    position = (len(sorted_values) - 1) * quantile
    lower = math.floor(position)
    upper = math.ceil(position)
    if lower == upper:
        return sorted_values[lower]
    weight = position - lower
    return sorted_values[lower] * (1 - weight) + sorted_values[upper] * weight


def round_optional(value: float | None) -> float | None:
    return None if value is None else round(value, 3)


def present_balance(values_by_tile: dict[str, list[float]]) -> dict[str, object]:
    counts = {tile: len(values) for tile, values in sorted(values_by_tile.items())}
    if not counts:
        return {"tile_present_counts": {}, "min": 0, "max": 0, "spread": 0}

    count_values = list(counts.values())
    return {
        "tile_present_counts": counts,
        "min": min(count_values),
        "max": max(count_values),
        "spread": max(count_values) - min(count_values),
    }


def analyze_log(path: Path) -> LogSummary:
    summary = LogSummary(path=str(path))

    with path.open("r", encoding="utf-8", errors="replace") as log_file:
        for line in log_file:
            if LOGICAL_FRAME_RE.search(line):
                summary.logical_frames += 1
            if METADATA_MATCH_RE.search(line):
                summary.metadata_matched += 1
            if METADATA_MISMATCH_RE.search(line):
                summary.metadata_mismatched += 1
            if "Wall repaint target skipped" in line:
                summary.skipped_repaint_targets += 1
            if "panic" in line:
                summary.panics += 1
            if re.search(r"\bERROR\b|\berror\b", line):
                summary.errors += 1
            if "missed_frame_count" in line:
                summary.missed_frame_logs += 1
            if re.search(r"requested frame .* still pending", line):
                summary.pending_frame_warnings += 1
            if "frame-ready-without-pending" in line:
                summary.unexpected_ready_logs += 1

            if match := RENDER_RE.search(line):
                render_ms = float(match.group("render_ms"))
                painter = match.group("painter")
                gpu = match.group("gpu")
                summary.render_ms.append(render_ms)
                summary.render_ms_by_painter[painter].append(render_ms)
                summary.requested_gpu_counts[gpu] += 1
                continue

            if match := REPAINT_RE.search(line):
                summary.repaint_render_ms.append(float(match.group("render_ms")))
                summary.target_present_ms.append(float(match.group("target_present_ms")))
                summary.requested_gpu_counts[match.group("gpu")] += 1
                continue

            if match := PRESENT_RE.search(line):
                tile = match.group("tile")
                gpu = match.group("gpu")
                present_ms = float(match.group("present_ms"))
                summary.window_present_ms.append(present_ms)
                summary.window_present_ms_by_tile[tile].append(present_ms)
                summary.tile_gpu_counts[f"tile={tile},gpu={gpu}"] += 1
                summary.requested_gpu_counts[match.group("requested_gpu")] += 1
                continue

            if match := BARRIER_COMPLETE_RE.search(line):
                summary.barrier_completed += 1
                summary.first_to_all_ready_ms.append(float(match.group("first_to_all_ready_ms")))
                summary.request_to_all_ready_ms.append(
                    float(match.group("request_to_all_ready_ms"))
                )
                summary.final_wait_ms.append(float(match.group("final_wait_ms")))
                continue

            if match := BARRIER_MISSED_RE.search(line):
                summary.barrier_missed += 1
                summary.missed_first_ready_elapsed_ms.append(
                    float(match.group("first_ready_elapsed_ms"))
                )
                continue

    return summary


def format_table_row(label: str, series: dict[str, object]) -> str:
    return (
        f"| {label} | {series['count']} | {series['avg_ms']} | {series['p50_ms']} | "
        f"{series['p95_ms']} | {series['p99_ms']} | {series['max_ms']} |"
    )


def to_markdown(results: list[dict[str, object]]) -> str:
    lines = ["# Wall Performance Summary", ""]
    for result in results:
        counts = result["counts"]
        series = result["series"]
        present = result["present_balance"]
        lines.extend(
            [
                f"## {Path(str(result['path'])).name}",
                "",
                f"- Logical frames: {counts['logical_frames']}",
                f"- Metadata matched/mismatched: {counts['metadata_matched']} / {counts['metadata_mismatched']}",
                f"- Barrier completed/missed: {counts['barrier_completed']} / {counts['barrier_missed']}",
                f"- Skipped repaint targets: {counts['skipped_repaint_targets']}",
                f"- Panic/error diagnostics: {counts['panics']} / {counts['errors']}",
                f"- Present balance: {present['tile_present_counts']} spread={present['spread']}",
                "",
                "| Series | Count | Avg ms | P50 ms | P95 ms | P99 ms | Max ms |",
                "| --- | ---: | ---: | ---: | ---: | ---: | ---: |",
                format_table_row("Painter render", series["painter_render"]),
                format_table_row("Repaint render", series["repaint_render"]),
                format_table_row("Window present", series["window_present"]),
                format_table_row("Barrier first-to-all-ready", series["barrier_first_to_all_ready"]),
                format_table_row("Barrier request-to-all-ready", series["barrier_request_to_all_ready"]),
                "",
                "### By Tile",
                "",
                "| Tile | Count | Avg present ms | P95 present ms | Max present ms |",
                "| --- | ---: | ---: | ---: | ---: |",
            ]
        )
        for tile, tile_series in result["by_tile"].items():
            lines.append(
                f"| {tile} | {tile_series['count']} | {tile_series['avg_ms']} | "
                f"{tile_series['p95_ms']} | {tile_series['max_ms']} |"
            )
        lines.append("")
    return "\n".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("logs", nargs="+", type=Path, help="Servo wall-layout stderr logs")
    parser.add_argument("--format", choices=("json", "markdown"), default="json")
    args = parser.parse_args()

    results = [analyze_log(path).to_dict() for path in args.logs]
    if args.format == "json":
        print(json.dumps({"logs": results}, indent=2))
    else:
        print(to_markdown(results))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
