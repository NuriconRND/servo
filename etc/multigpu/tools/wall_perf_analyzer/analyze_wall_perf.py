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
import sys
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
MEDIA_FRAME_RE = re.compile(
    r"Wall media frame: .*?frame_backend=(?P<backend>\w+) .*?"
    r"image_update=(?P<image_update>\w+) .*?"
    r"delete_updates=(?P<delete_updates>\d+) updates_total=(?P<updates_total>\d+)"
)
MEDIA_FANOUT_RE = re.compile(
    r"Wall media image fanout: .*?target_painters=(?P<target_painters>\[.*?\]) "
    r"requested_gpus=(?P<requested_gpus>\[.*?\]) "
    r"updates_total=(?P<updates_total>\d+) adds=(?P<adds>\d+) "
    r"updates=(?P<updates>\d+) deletes=(?P<deletes>\d+) "
    r"animation_updates=(?P<animation_updates>\d+)"
)


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
    media_frames: int = 0
    media_raw_frames: int = 0
    media_gl_texture_frames: int = 0
    media_external_oes_frames: int = 0
    media_image_adds: int = 0
    media_image_updates: int = 0
    media_image_noops: int = 0
    media_image_deletes: int = 0
    media_image_update_messages: int = 0
    media_image_fanouts: int = 0
    media_fanout_updates_total: int = 0
    media_fanout_adds: int = 0
    media_fanout_updates: int = 0
    media_fanout_deletes: int = 0
    media_fanout_animation_updates: int = 0
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
    media_fanout_requested_gpu_counts: Counter[str] = field(default_factory=Counter)

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
                "media_frames": self.media_frames,
                "media_raw_frames": self.media_raw_frames,
                "media_gl_texture_frames": self.media_gl_texture_frames,
                "media_external_oes_frames": self.media_external_oes_frames,
                "media_image_adds": self.media_image_adds,
                "media_image_updates": self.media_image_updates,
                "media_image_noops": self.media_image_noops,
                "media_image_deletes": self.media_image_deletes,
                "media_image_update_messages": self.media_image_update_messages,
                "media_image_fanouts": self.media_image_fanouts,
                "media_fanout_updates_total": self.media_fanout_updates_total,
                "media_fanout_adds": self.media_fanout_adds,
                "media_fanout_updates": self.media_fanout_updates,
                "media_fanout_deletes": self.media_fanout_deletes,
                "media_fanout_animation_updates": self.media_fanout_animation_updates,
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
            "media_fanout_requested_gpu_counts": dict(
                sorted(self.media_fanout_requested_gpu_counts.items())
            ),
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

            if match := MEDIA_FRAME_RE.search(line):
                backend = match.group("backend")
                image_update = match.group("image_update")
                summary.media_frames += 1
                if backend == "raw":
                    summary.media_raw_frames += 1
                elif backend == "gl_texture":
                    summary.media_gl_texture_frames += 1
                elif backend == "external_oes":
                    summary.media_external_oes_frames += 1

                if image_update == "add":
                    summary.media_image_adds += 1
                elif image_update == "update":
                    summary.media_image_updates += 1
                elif image_update == "none":
                    summary.media_image_noops += 1
                summary.media_image_deletes += int(match.group("delete_updates"))
                summary.media_image_update_messages += int(match.group("updates_total"))
                continue

            if match := MEDIA_FANOUT_RE.search(line):
                summary.media_image_fanouts += 1
                summary.media_fanout_updates_total += int(match.group("updates_total"))
                summary.media_fanout_adds += int(match.group("adds"))
                summary.media_fanout_updates += int(match.group("updates"))
                summary.media_fanout_deletes += int(match.group("deletes"))
                summary.media_fanout_animation_updates += int(match.group("animation_updates"))
                for requested_gpu in re.findall(
                    r"Some\(\d+\)|None", match.group("requested_gpus")
                ):
                    summary.media_fanout_requested_gpu_counts[requested_gpu] += 1
                continue

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
                (
                    f"- Media frames raw/gl/external_oes: {counts['media_raw_frames']} / "
                    f"{counts['media_gl_texture_frames']} / {counts['media_external_oes_frames']}"
                ),
                (
                    f"- Media image add/update/noop/delete messages: "
                    f"{counts['media_image_adds']} / {counts['media_image_updates']} / "
                    f"{counts['media_image_noops']} / {counts['media_image_deletes']}"
                ),
                (
                    f"- Media fan-outs: {counts['media_image_fanouts']} "
                    f"updates_total={counts['media_fanout_updates_total']}"
                ),
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


def requested_gpu_ids(result: dict[str, object]) -> set[str]:
    gpu_ids = set()
    for counts_key in ("requested_gpu_counts", "media_fanout_requested_gpu_counts"):
        for requested_gpu in result[counts_key]:
            if match := re.fullmatch(r"Some\((\d+)\)", requested_gpu):
                gpu_ids.add(match.group(1))
    return gpu_ids


def validate_media_wall_result(
    result: dict[str, object],
    expected_gpus: list[str],
    max_present_spread: int,
) -> list[str]:
    counts = result["counts"]
    present = result["present_balance"]
    failures = []

    if counts["panics"] > 0:
        failures.append(f"panic diagnostics present: {counts['panics']}")
    if counts["errors"] > 0:
        failures.append(f"error diagnostics present: {counts['errors']}")
    if counts["metadata_mismatched"] > 0:
        failures.append(f"metadata mismatches present: {counts['metadata_mismatched']}")
    if counts["media_frames"] == 0:
        failures.append("no media frame diagnostics found")
    if counts["media_image_fanouts"] == 0:
        failures.append("no media image fan-out diagnostics found")
    if present["spread"] > max_present_spread:
        failures.append(
            f"present count spread {present['spread']} exceeds {max_present_spread}"
        )

    if expected_gpus:
        actual_gpus = requested_gpu_ids(result)
        missing_gpus = sorted(set(expected_gpus) - actual_gpus)
        if missing_gpus:
            failures.append(
                "missing expected requested GPU ids: " + ", ".join(missing_gpus)
            )

    return failures


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("logs", nargs="+", type=Path, help="Servo wall-layout stderr logs")
    parser.add_argument("--format", choices=("json", "markdown"), default="json")
    parser.add_argument(
        "--validate-media-wall",
        action="store_true",
        help="fail if media frame fan-out wall diagnostics do not satisfy the smoke gate",
    )
    parser.add_argument(
        "--expected-gpu",
        action="append",
        default=[],
        help="requested GPU id expected in the log; repeat for multiple ids",
    )
    parser.add_argument(
        "--max-present-spread",
        type=int,
        default=2,
        help="maximum allowed difference between per-tile present counts",
    )
    args = parser.parse_args()

    results = [analyze_log(path).to_dict() for path in args.logs]
    if args.format == "json":
        print(json.dumps({"logs": results}, indent=2))
    else:
        print(to_markdown(results))

    if args.validate_media_wall:
        validation_failures = {
            str(result["path"]): validate_media_wall_result(
                result, args.expected_gpu, args.max_present_spread
            )
            for result in results
        }
        failed = False
        for path, failures in validation_failures.items():
            if not failures:
                continue
            failed = True
            print(f"{path}: media wall validation failed", file=sys.stderr)
            for failure in failures:
                print(f"  - {failure}", file=sys.stderr)
        if failed:
            return 1

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
