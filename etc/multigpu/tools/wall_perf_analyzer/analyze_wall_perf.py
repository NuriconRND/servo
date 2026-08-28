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


OPTIONAL_MS_PATTERN = r"None|Some\(-?[0-9.]+\)|-?[0-9.]+"
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
    r"Wall window present: .*? tile=(?P<tile>\d+) "
    r"(?:display=(?P<display>\d+)|monitor=(?P<monitor>\d+) gpu=(?P<gpu>\d+)) "
    r"requested_gpu=(?P<requested_gpu>None|Some\((?P<requested_gpu_id>\d+)\)) "
    r"present_ms=(?P<present_ms>[0-9.]+)"
)
FRAME_REQUEST_RE = re.compile(
    r"Wall frame requested: .*?pending_before=(?P<pending_before>\d+)"
)
FRAME_OVERLAP_RE = re.compile(r"Wall frame overlap:")
PACING_EVENT_RE = re.compile(
    r"Wall frame pacing (?P<event>coalesced|released): .*?"
    r"pending_max=(?P<pending_max>\d+) "
    r"coalesced_for_webview=(?P<coalesced_for_webview>\d+) "
    r"total_(?P<total_kind>coalesced|released)=(?P<event_total>\d+) "
    r"policy=(?P<policy>\S+)"
)
PACING_SUMMARY_RE = re.compile(
    r"Wall frame pacing summary: event=(?P<event>\w+) .*?"
    r"pending_max=(?P<pending_max>\d+) "
    r"coalesced_for_webview=(?P<coalesced_for_webview>\d+) "
    r"total_coalesced=(?P<total_coalesced>\d+) "
    r"total_released=(?P<total_released>\d+) "
    r"policy=(?P<policy>\S+)"
    # Optional on purpose: logs captured before the per-reason tally existed must keep
    # parsing, otherwise every archived run silently reports zero pacing summaries.
    r"(?: blocked_by_active=(?P<blocked_active>\d+)"
    r" blocked_by_pending=(?P<blocked_pending>\d+)"
    r" blocked_by_min_interval=(?P<blocked_min_interval>\d+))?"
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
    r"Wall media frame: .*?media_frame_id=(?P<frame_id>\d+) .*?"
    r"frame_backend=(?P<backend>\w+) .*?"
    r"image_update=(?P<image_update>\w+) .*?"
    r"delete_updates=(?P<delete_updates>\d+) updates_total=(?P<updates_total>\d+)"
    r"(?: .*?inter_frame_ms=(?P<inter_frame_ms>None|Some\([0-9.]+\)|[0-9.]+))?"
)
MEDIA_FRAME_SUMMARY_RE = re.compile(
    r"Wall media frame summary: .*?media_frame_id=(?P<frame_id>\d+) .*?"
    r"frame_backend=(?P<backend>\w+) .*?"
    r"image_update=(?P<image_update>\w+) frames_since_summary=(?P<frames>\d+) .*?"
    r"summary_elapsed_ms=(?P<summary_elapsed_ms>[0-9.]+) "
    r"delete_updates=(?P<delete_updates>\d+) updates_total=(?P<updates_total>\d+)"
    r"(?: .*?inter_frame_ms=(?P<inter_frame_ms>None|Some\([0-9.]+\)|[0-9.]+))?"
)
MEDIA_FANOUT_RE = re.compile(
    r"Wall media image fanout: .*?target_painters=(?P<target_painters>\[.*?\]) "
    r"requested_gpus=(?P<requested_gpus>\[.*?\]) "
    r"updates_total=(?P<updates_total>\d+) adds=(?P<adds>\d+) "
    r"updates=(?P<updates>\d+) deletes=(?P<deletes>\d+) "
    r"animation_updates=(?P<animation_updates>\d+)"
)
MEDIA_FANOUT_SUMMARY_RE = re.compile(
    r"Wall media image fanout summary: fanout_id=(?P<fanout_id>\d+) .*?"
    r"target_painters=(?P<target_painters>\[.*?\]) "
    r"requested_gpus=(?P<requested_gpus>\[.*?\]) "
    r"updates_total=(?P<updates_total>\d+) adds=(?P<adds>\d+) "
    r"updates=(?P<updates>\d+) deletes=(?P<deletes>\d+) "
    r"animation_updates=(?P<animation_updates>\d+)"
)
RAW_VIDEO_PLANE_LOCK_RE = re.compile(
    r"Wall raw video plane lock: external_id=(?P<external_id>\d+) "
    r"plane_index=(?P<plane_index>\d+) size=(?P<width>\d+)x(?P<height>\d+) "
    r"bytes=(?P<bytes>\d+) total_locks=(?P<total_locks>\d+)"
)
RAW_VIDEO_PLANE_UNLOCK_RE = re.compile(
    r"Wall raw video plane unlock: external_id=(?P<external_id>\d+) "
    r"plane_index=(?P<plane_index>\d+) total_unlocks=(?P<total_unlocks>\d+)"
)
RAW_VIDEO_PLANE_LOCK_SUMMARY_RE = re.compile(
    r"Wall raw video plane lock summary: total_locks=(?P<total_locks>\d+)"
)
RAW_VIDEO_PLANE_UNLOCK_SUMMARY_RE = re.compile(
    r"Wall raw video plane unlock summary: total_unlocks=(?P<total_unlocks>\d+)"
)
GST_VIDEO_SAMPLE_RE = re.compile(
    rf"GStreamer video sample: sample_id=(?P<sample_id>\d+) "
    rf"pts_ms=(?P<pts_ms>{OPTIONAL_MS_PATTERN}) "
    rf"duration_ms=(?P<duration_ms>{OPTIONAL_MS_PATTERN}) "
    rf"pts_delta_ms=(?P<pts_delta_ms>{OPTIONAL_MS_PATTERN}) "
    rf"wall_delta_ms=(?P<wall_delta_ms>{OPTIONAL_MS_PATTERN}) "
    rf"pts_gap_ms=(?P<pts_gap_ms>{OPTIONAL_MS_PATTERN}) "
    r"late_pts_gap=(?P<late_pts_gap>true|false) "
    r"late_wall_gap=(?P<late_wall_gap>true|false)"
)
GST_VIDEO_SAMPLE_SUMMARY_RE = re.compile(
    rf"GStreamer video sample summary: sample_id=(?P<sample_id>\d+) "
    r"frames_since_summary=(?P<frames>\d+) "
    r"summary_wall_elapsed_ms=(?P<summary_wall_elapsed_ms>[0-9.]+) "
    rf"pts_ms=(?P<pts_ms>{OPTIONAL_MS_PATTERN}) "
    rf"duration_ms=(?P<duration_ms>{OPTIONAL_MS_PATTERN}) "
    rf"pts_delta_ms=(?P<pts_delta_ms>{OPTIONAL_MS_PATTERN}) "
    rf"wall_delta_ms=(?P<wall_delta_ms>{OPTIONAL_MS_PATTERN}) "
    rf"pts_gap_ms=(?P<pts_gap_ms>{OPTIONAL_MS_PATTERN}) "
    r"late_pts_gaps=(?P<late_pts_gaps>\d+) "
    r"late_wall_gaps=(?P<late_wall_gaps>\d+)"
)
NONFATAL_ERROR_RE = re.compile(
    r"audiosink .* error writing data|"
    r"error writing data in gst_wasapi_sink_write|"
    r"ERROR\s+libav .* co located POCs unavailable",
    re.IGNORECASE,
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
    nonfatal_errors: int = 0
    missed_frame_logs: int = 0
    pending_frame_warnings: int = 0
    unexpected_ready_logs: int = 0
    wall_frame_requests: int = 0
    wall_frame_pending_nonzero: int = 0
    wall_frame_max_pending_before: int = 0
    wall_frame_overlaps: int = 0
    wall_frame_pacing_coalesced: int = 0
    wall_frame_pacing_released: int = 0
    wall_frame_pacing_summaries: int = 0
    wall_frame_pacing_max_pending: int = 0
    # Which gate blocked. Counts the FIRST matching gate (Active -> Pending -> TooSoon), so
    # an earlier gate masks the later ones -- a zero does not prove that gate never binds.
    wall_frame_pacing_blocked_active: int = 0
    wall_frame_pacing_blocked_pending: int = 0
    wall_frame_pacing_blocked_min_interval: int = 0
    media_frames: int = 0
    media_frame_summaries: int = 0
    media_frame_summary_frames: int = 0
    media_frame_summary_last_id: int = 0
    media_raw_frames: int = 0
    media_gl_texture_frames: int = 0
    media_external_oes_frames: int = 0
    media_yuv_i420_frames: int = 0
    media_yuv_nv12_frames: int = 0
    media_summary_raw_frames: int = 0
    media_summary_gl_texture_frames: int = 0
    media_summary_external_oes_frames: int = 0
    media_summary_yuv_i420_frames: int = 0
    media_summary_yuv_nv12_frames: int = 0
    duplicate_media_frame_ids: int = 0
    media_stale_frame_gaps: int = 0
    media_image_adds: int = 0
    media_image_updates: int = 0
    media_image_noops: int = 0
    media_image_deletes: int = 0
    media_image_update_messages: int = 0
    media_image_fanouts: int = 0
    media_image_fanout_summaries: int = 0
    media_image_fanout_summary_last_id: int = 0
    media_fanout_updates_total: int = 0
    media_fanout_adds: int = 0
    media_fanout_updates: int = 0
    media_fanout_deletes: int = 0
    media_fanout_animation_updates: int = 0
    raw_yuv_plane_locks: int = 0
    raw_yuv_plane_unlocks: int = 0
    raw_yuv_plane_lock_summary_total: int = 0
    raw_yuv_plane_unlock_summary_total: int = 0
    gst_video_samples: int = 0
    gst_video_sample_summaries: int = 0
    gst_video_sample_summary_frames: int = 0
    gst_video_sample_summary_last_id: int = 0
    gst_video_sample_late_pts_gaps: int = 0
    gst_video_sample_late_wall_gaps: int = 0
    gst_video_summary_late_pts_gaps: int = 0
    gst_video_summary_late_wall_gaps: int = 0
    render_ms: list[float] = field(default_factory=list)
    repaint_render_ms: list[float] = field(default_factory=list)
    target_present_ms: list[float] = field(default_factory=list)
    window_present_ms: list[float] = field(default_factory=list)
    media_inter_frame_ms: list[float] = field(default_factory=list)
    media_summary_avg_frame_ms: list[float] = field(default_factory=list)
    gst_video_sample_pts_delta_ms: list[float] = field(default_factory=list)
    gst_video_sample_wall_delta_ms: list[float] = field(default_factory=list)
    gst_video_sample_pts_gap_ms: list[float] = field(default_factory=list)
    gst_video_summary_pts_delta_ms: list[float] = field(default_factory=list)
    gst_video_summary_wall_delta_ms: list[float] = field(default_factory=list)
    gst_video_summary_pts_gap_ms: list[float] = field(default_factory=list)
    gst_video_summary_avg_wall_ms: list[float] = field(default_factory=list)
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
    seen_media_frame_ids: set[int] = field(default_factory=set)

    def to_dict(self) -> dict[str, object]:
        observed_media_frames = self.media_frames or self.media_frame_summary_frames
        observed_media_raw_frames = self.media_raw_frames or self.media_summary_raw_frames
        observed_media_gl_texture_frames = (
            self.media_gl_texture_frames or self.media_summary_gl_texture_frames
        )
        observed_media_external_oes_frames = (
            self.media_external_oes_frames or self.media_summary_external_oes_frames
        )
        observed_media_yuv_i420_frames = (
            self.media_yuv_i420_frames or self.media_summary_yuv_i420_frames
        )
        observed_media_yuv_nv12_frames = (
            self.media_yuv_nv12_frames or self.media_summary_yuv_nv12_frames
        )
        observed_media_image_fanouts = (
            self.media_image_fanouts or self.media_image_fanout_summary_last_id
        )
        observed_raw_yuv_plane_locks = max(
            self.raw_yuv_plane_locks, self.raw_yuv_plane_lock_summary_total
        )
        observed_raw_yuv_plane_unlocks = max(
            self.raw_yuv_plane_unlocks, self.raw_yuv_plane_unlock_summary_total
        )
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
                "nonfatal_errors": self.nonfatal_errors,
                "missed_frame_logs": self.missed_frame_logs,
                "pending_frame_warnings": self.pending_frame_warnings,
                "unexpected_ready_logs": self.unexpected_ready_logs,
                "wall_frame_requests": self.wall_frame_requests,
                "wall_frame_pending_nonzero": self.wall_frame_pending_nonzero,
                "wall_frame_max_pending_before": self.wall_frame_max_pending_before,
                "wall_frame_overlaps": self.wall_frame_overlaps,
                "wall_frame_pacing_coalesced": self.wall_frame_pacing_coalesced,
                "wall_frame_pacing_released": self.wall_frame_pacing_released,
                "wall_frame_pacing_summaries": self.wall_frame_pacing_summaries,
                "wall_frame_pacing_max_pending": self.wall_frame_pacing_max_pending,
                "wall_frame_pacing_blocked_active": self.wall_frame_pacing_blocked_active,
                "wall_frame_pacing_blocked_pending": self.wall_frame_pacing_blocked_pending,
                "wall_frame_pacing_blocked_min_interval": (
                    self.wall_frame_pacing_blocked_min_interval
                ),
                "media_frames": self.media_frames,
                "media_frame_summaries": self.media_frame_summaries,
                "media_frame_summary_frames": self.media_frame_summary_frames,
                "media_frame_summary_last_id": self.media_frame_summary_last_id,
                "media_frames_observed": observed_media_frames,
                "media_raw_frames": self.media_raw_frames,
                "media_gl_texture_frames": self.media_gl_texture_frames,
                "media_external_oes_frames": self.media_external_oes_frames,
                "media_yuv_i420_frames": self.media_yuv_i420_frames,
                "media_yuv_nv12_frames": self.media_yuv_nv12_frames,
                "media_raw_frames_observed": observed_media_raw_frames,
                "media_gl_texture_frames_observed": observed_media_gl_texture_frames,
                "media_external_oes_frames_observed": observed_media_external_oes_frames,
                "media_yuv_i420_frames_observed": observed_media_yuv_i420_frames,
                "media_yuv_nv12_frames_observed": observed_media_yuv_nv12_frames,
                "duplicate_media_frame_ids": self.duplicate_media_frame_ids,
                "media_stale_frame_gaps": self.media_stale_frame_gaps,
                "media_image_adds": self.media_image_adds,
                "media_image_updates": self.media_image_updates,
                "media_image_noops": self.media_image_noops,
                "media_image_deletes": self.media_image_deletes,
                "media_image_update_messages": self.media_image_update_messages,
                "media_image_fanouts": self.media_image_fanouts,
                "media_image_fanout_summaries": self.media_image_fanout_summaries,
                "media_image_fanout_summary_last_id": self.media_image_fanout_summary_last_id,
                "media_image_fanouts_observed": observed_media_image_fanouts,
                "media_fanout_updates_total": self.media_fanout_updates_total,
                "media_fanout_adds": self.media_fanout_adds,
                "media_fanout_updates": self.media_fanout_updates,
                "media_fanout_deletes": self.media_fanout_deletes,
                "media_fanout_animation_updates": self.media_fanout_animation_updates,
                "raw_yuv_plane_locks": self.raw_yuv_plane_locks,
                "raw_yuv_plane_unlocks": self.raw_yuv_plane_unlocks,
                "raw_yuv_plane_locks_observed": observed_raw_yuv_plane_locks,
                "raw_yuv_plane_unlocks_observed": observed_raw_yuv_plane_unlocks,
                "gst_video_samples": self.gst_video_samples,
                "gst_video_sample_summaries": self.gst_video_sample_summaries,
                "gst_video_sample_summary_frames": self.gst_video_sample_summary_frames,
                "gst_video_sample_summary_last_id": self.gst_video_sample_summary_last_id,
                "gst_video_sample_late_pts_gaps": self.gst_video_sample_late_pts_gaps,
                "gst_video_sample_late_wall_gaps": self.gst_video_sample_late_wall_gaps,
                "gst_video_summary_late_pts_gaps": self.gst_video_summary_late_pts_gaps,
                "gst_video_summary_late_wall_gaps": self.gst_video_summary_late_wall_gaps,
            },
            "series": {
                "painter_render": SeriesSummary.from_values(self.render_ms).to_dict(),
                "repaint_render": SeriesSummary.from_values(self.repaint_render_ms).to_dict(),
                "target_present": SeriesSummary.from_values(self.target_present_ms).to_dict(),
                "window_present": SeriesSummary.from_values(self.window_present_ms).to_dict(),
                "media_inter_frame": SeriesSummary.from_values(
                    self.media_inter_frame_ms
                ).to_dict(),
                "media_summary_avg_frame": SeriesSummary.from_values(
                    self.media_summary_avg_frame_ms
                ).to_dict(),
                "gst_video_sample_pts_delta": SeriesSummary.from_values(
                    self.gst_video_sample_pts_delta_ms
                ).to_dict(),
                "gst_video_sample_wall_delta": SeriesSummary.from_values(
                    self.gst_video_sample_wall_delta_ms
                ).to_dict(),
                "gst_video_sample_pts_gap": SeriesSummary.from_values(
                    self.gst_video_sample_pts_gap_ms
                ).to_dict(),
                "gst_video_summary_pts_delta": SeriesSummary.from_values(
                    self.gst_video_summary_pts_delta_ms
                ).to_dict(),
                "gst_video_summary_wall_delta": SeriesSummary.from_values(
                    self.gst_video_summary_wall_delta_ms
                ).to_dict(),
                "gst_video_summary_pts_gap": SeriesSummary.from_values(
                    self.gst_video_summary_pts_gap_ms
                ).to_dict(),
                "gst_video_summary_avg_wall": SeriesSummary.from_values(
                    self.gst_video_summary_avg_wall_ms
                ).to_dict(),
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


def parse_optional_ms(value: str | None) -> float | None:
    if not value or value == "None":
        return None
    if value.startswith("Some(") and value.endswith(")"):
        value = value[5:-1]
    return float(value)


def append_optional_ms(values: list[float], value: str | None) -> None:
    parsed = parse_optional_ms(value)
    if parsed is not None:
        values.append(parsed)


def add_backend_count(summary: LogSummary, backend: str, count: int, *, from_summary: bool) -> None:
    if backend == "raw":
        if from_summary:
            summary.media_summary_raw_frames += count
        else:
            summary.media_raw_frames += count
    elif backend == "gl_texture":
        if from_summary:
            summary.media_summary_gl_texture_frames += count
        else:
            summary.media_gl_texture_frames += count
    elif backend == "external_oes":
        if from_summary:
            summary.media_summary_external_oes_frames += count
        else:
            summary.media_external_oes_frames += count
    elif backend == "yuv_i420_external_raw":
        if from_summary:
            summary.media_summary_yuv_i420_frames += count
        else:
            summary.media_yuv_i420_frames += count
    elif backend == "yuv_nv12_external_raw":
        if from_summary:
            summary.media_summary_yuv_nv12_frames += count
        else:
            summary.media_yuv_nv12_frames += count


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
            if NONFATAL_ERROR_RE.search(line):
                summary.nonfatal_errors += 1
            elif re.search(r"\bERROR\b|\berror\b", line):
                summary.errors += 1
            if "missed_frame_count" in line:
                summary.missed_frame_logs += 1
            if re.search(r"requested frame .* still pending", line):
                summary.pending_frame_warnings += 1
            if "frame-ready-without-pending" in line:
                summary.unexpected_ready_logs += 1
            if FRAME_OVERLAP_RE.search(line):
                summary.wall_frame_overlaps += 1
            if match := FRAME_REQUEST_RE.search(line):
                pending_before = int(match.group("pending_before"))
                summary.wall_frame_requests += 1
                summary.wall_frame_max_pending_before = max(
                    summary.wall_frame_max_pending_before,
                    pending_before,
                )
                if pending_before > 0:
                    summary.wall_frame_pending_nonzero += 1
            if match := PACING_EVENT_RE.search(line):
                pending_max = int(match.group("pending_max"))
                event_total = int(match.group("event_total"))
                summary.wall_frame_pacing_max_pending = max(
                    summary.wall_frame_pacing_max_pending,
                    pending_max,
                )
                if match.group("event") == "coalesced":
                    summary.wall_frame_pacing_coalesced = max(
                        summary.wall_frame_pacing_coalesced,
                        event_total,
                    )
                elif match.group("event") == "released":
                    summary.wall_frame_pacing_released = max(
                        summary.wall_frame_pacing_released,
                        event_total,
                    )
            if match := PACING_SUMMARY_RE.search(line):
                summary.wall_frame_pacing_summaries += 1
                summary.wall_frame_pacing_max_pending = max(
                    summary.wall_frame_pacing_max_pending,
                    int(match.group("pending_max")),
                )
                summary.wall_frame_pacing_coalesced = max(
                    summary.wall_frame_pacing_coalesced,
                    int(match.group("total_coalesced")),
                )
                summary.wall_frame_pacing_released = max(
                    summary.wall_frame_pacing_released,
                        int(match.group("total_released")),
                    )
                # Cumulative in the log, so take the highest seen rather than summing.
                for group, field in (
                    ("blocked_active", "wall_frame_pacing_blocked_active"),
                    ("blocked_pending", "wall_frame_pacing_blocked_pending"),
                    ("blocked_min_interval", "wall_frame_pacing_blocked_min_interval"),
                ):
                    value = match.group(group)
                    if value is not None:
                        setattr(summary, field, max(getattr(summary, field), int(value)))

            if match := GST_VIDEO_SAMPLE_RE.search(line):
                summary.gst_video_samples += 1
                if match.group("late_pts_gap") == "true":
                    summary.gst_video_sample_late_pts_gaps += 1
                if match.group("late_wall_gap") == "true":
                    summary.gst_video_sample_late_wall_gaps += 1
                append_optional_ms(
                    summary.gst_video_sample_pts_delta_ms,
                    match.group("pts_delta_ms"),
                )
                append_optional_ms(
                    summary.gst_video_sample_wall_delta_ms,
                    match.group("wall_delta_ms"),
                )
                append_optional_ms(
                    summary.gst_video_sample_pts_gap_ms,
                    match.group("pts_gap_ms"),
                )
                continue

            if match := GST_VIDEO_SAMPLE_SUMMARY_RE.search(line):
                frames = int(match.group("frames"))
                summary.gst_video_sample_summaries += 1
                summary.gst_video_sample_summary_frames += frames
                summary.gst_video_sample_summary_last_id = max(
                    summary.gst_video_sample_summary_last_id,
                    int(match.group("sample_id")),
                )
                summary.gst_video_summary_late_pts_gaps += int(
                    match.group("late_pts_gaps")
                )
                summary.gst_video_summary_late_wall_gaps += int(
                    match.group("late_wall_gaps")
                )
                if frames > 0:
                    summary.gst_video_summary_avg_wall_ms.append(
                        float(match.group("summary_wall_elapsed_ms")) / frames
                    )
                append_optional_ms(
                    summary.gst_video_summary_pts_delta_ms,
                    match.group("pts_delta_ms"),
                )
                append_optional_ms(
                    summary.gst_video_summary_wall_delta_ms,
                    match.group("wall_delta_ms"),
                )
                append_optional_ms(
                    summary.gst_video_summary_pts_gap_ms,
                    match.group("pts_gap_ms"),
                )
                continue

            if match := MEDIA_FRAME_RE.search(line):
                frame_id = int(match.group("frame_id"))
                backend = match.group("backend")
                image_update = match.group("image_update")
                summary.media_frames += 1
                if frame_id in summary.seen_media_frame_ids:
                    summary.duplicate_media_frame_ids += 1
                summary.seen_media_frame_ids.add(frame_id)
                add_backend_count(summary, backend, 1, from_summary=False)

                if image_update == "add":
                    summary.media_image_adds += 1
                elif image_update == "update":
                    summary.media_image_updates += 1
                elif image_update == "none":
                    summary.media_image_noops += 1
                summary.media_image_deletes += int(match.group("delete_updates"))
                summary.media_image_update_messages += int(match.group("updates_total"))
                inter_frame_ms = parse_optional_ms(match.group("inter_frame_ms"))
                if inter_frame_ms is not None:
                    summary.media_inter_frame_ms.append(inter_frame_ms)
                    if inter_frame_ms > 33.334:
                        summary.media_stale_frame_gaps += 1
                continue

            if match := MEDIA_FRAME_SUMMARY_RE.search(line):
                frame_id = int(match.group("frame_id"))
                frames = int(match.group("frames"))
                backend = match.group("backend")
                summary.media_frame_summaries += 1
                summary.media_frame_summary_frames += frames
                summary.media_frame_summary_last_id = max(summary.media_frame_summary_last_id, frame_id)
                if frames > 0 and match.group("image_update") == "update":
                    summary.media_summary_avg_frame_ms.append(
                        float(match.group("summary_elapsed_ms")) / frames
                    )
                add_backend_count(summary, backend, frames, from_summary=True)
                inter_frame_ms = parse_optional_ms(match.group("inter_frame_ms"))
                if inter_frame_ms is not None:
                    summary.media_inter_frame_ms.append(inter_frame_ms)
                    if inter_frame_ms > 33.334:
                        summary.media_stale_frame_gaps += 1
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

            if match := MEDIA_FANOUT_SUMMARY_RE.search(line):
                fanout_id = int(match.group("fanout_id"))
                summary.media_image_fanout_summaries += 1
                summary.media_image_fanout_summary_last_id = max(
                    summary.media_image_fanout_summary_last_id, fanout_id
                )
                for requested_gpu in re.findall(
                    r"Some\(\d+\)|None", match.group("requested_gpus")
                ):
                    summary.media_fanout_requested_gpu_counts[requested_gpu] += 1
                continue

            if match := RAW_VIDEO_PLANE_LOCK_RE.search(line):
                summary.raw_yuv_plane_locks += 1
                summary.raw_yuv_plane_lock_summary_total = max(
                    summary.raw_yuv_plane_lock_summary_total,
                    int(match.group("total_locks")),
                )
                continue

            if match := RAW_VIDEO_PLANE_UNLOCK_RE.search(line):
                summary.raw_yuv_plane_unlocks += 1
                summary.raw_yuv_plane_unlock_summary_total = max(
                    summary.raw_yuv_plane_unlock_summary_total,
                    int(match.group("total_unlocks")),
                )
                continue

            if match := RAW_VIDEO_PLANE_LOCK_SUMMARY_RE.search(line):
                summary.raw_yuv_plane_lock_summary_total = max(
                    summary.raw_yuv_plane_lock_summary_total,
                    int(match.group("total_locks")),
                )
                continue

            if match := RAW_VIDEO_PLANE_UNLOCK_SUMMARY_RE.search(line):
                summary.raw_yuv_plane_unlock_summary_total = max(
                    summary.raw_yuv_plane_unlock_summary_total,
                    int(match.group("total_unlocks")),
                )
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
                # New-format logs carry `display=N` (spatial display index); archived
                # old-format logs carry `monitor=N gpu=N` instead. Prefer `display` when
                # present, and fall back to the legacy `gpu` group so `tile_gpu_counts`
                # aggregation keeps working against archived logs.
                display = match.group("display")
                if display is not None:
                    tile_gpu_label = f"tile={tile},display={display}"
                else:
                    tile_gpu_label = f"tile={tile},gpu={match.group('gpu')}"
                present_ms = float(match.group("present_ms"))
                summary.window_present_ms.append(present_ms)
                summary.window_present_ms_by_tile[tile].append(present_ms)
                summary.tile_gpu_counts[tile_gpu_label] += 1
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
                (
                    f"- Panic/error diagnostics: {counts['panics']} / {counts['errors']} "
                    f"(nonfatal media warnings: {counts['nonfatal_errors']})"
                ),
                (
                    f"- Wall frame requests/overlaps/pending-nonzero: "
                    f"{counts['wall_frame_requests']} / {counts['wall_frame_overlaps']} / "
                    f"{counts['wall_frame_pending_nonzero']} "
                    f"(max_pending_before={counts['wall_frame_max_pending_before']})"
                ),
                (
                    f"- Wall frame pacing coalesced/released: "
                    f"{counts['wall_frame_pacing_coalesced']} / "
                    f"{counts['wall_frame_pacing_released']} "
                    f"(summaries={counts['wall_frame_pacing_summaries']}, "
                    f"max_pending={counts['wall_frame_pacing_max_pending']})"
                ),
                (
                    f"- Wall frame pacing blocked by "
                    f"active/pending/min_interval: "
                    f"{counts['wall_frame_pacing_blocked_active']} / "
                    f"{counts['wall_frame_pacing_blocked_pending']} / "
                    f"{counts['wall_frame_pacing_blocked_min_interval']} "
                    f"(first matching gate only; an earlier gate masks later ones)"
                ),
                f"- Present balance: {present['tile_present_counts']} spread={present['spread']}",
                (
                    f"- Media frames observed raw/gl/external_oes: "
                    f"{counts['media_raw_frames_observed']} / "
                    f"{counts['media_gl_texture_frames_observed']} / "
                    f"{counts['media_external_oes_frames_observed']}"
                ),
                (
                    f"- Media frames observed yuv_i420/yuv_nv12: "
                    f"{counts['media_yuv_i420_frames_observed']} / "
                    f"{counts['media_yuv_nv12_frames_observed']}"
                ),
                (
                    f"- Media frame details/summaries: {counts['media_frames']} / "
                    f"{counts['media_frame_summaries']} "
                    f"(summary frames={counts['media_frame_summary_frames']})"
                ),
                (
                    f"- Media image add/update/noop/delete messages: "
                    f"{counts['media_image_adds']} / {counts['media_image_updates']} / "
                    f"{counts['media_image_noops']} / {counts['media_image_deletes']}"
                ),
                (
                    f"- Media fan-outs observed: {counts['media_image_fanouts_observed']} "
                    f"(details={counts['media_image_fanouts']}, "
                    f"summaries={counts['media_image_fanout_summaries']}) "
                    f"updates_total={counts['media_fanout_updates_total']}"
                ),
                (
                    f"- Raw YUV plane locks/unlocks observed: "
                    f"{counts['raw_yuv_plane_locks_observed']} / "
                    f"{counts['raw_yuv_plane_unlocks_observed']}"
                ),
                (
                    f"- GStreamer video sample details/summaries: "
                    f"{counts['gst_video_samples']} / "
                    f"{counts['gst_video_sample_summaries']} "
                    f"(summary frames={counts['gst_video_sample_summary_frames']})"
                ),
                (
                    f"- GStreamer video late pts/wall gaps detail: "
                    f"{counts['gst_video_sample_late_pts_gaps']} / "
                    f"{counts['gst_video_sample_late_wall_gaps']} "
                    f"(summary: {counts['gst_video_summary_late_pts_gaps']} / "
                    f"{counts['gst_video_summary_late_wall_gaps']})"
                ),
                (
                    f"- Duplicate media frame ids / stale frame gaps: "
                    f"{counts['duplicate_media_frame_ids']} / {counts['media_stale_frame_gaps']}"
                ),
                "",
                "| Series | Count | Avg ms | P50 ms | P95 ms | P99 ms | Max ms |",
                "| --- | ---: | ---: | ---: | ---: | ---: | ---: |",
                format_table_row("Painter render", series["painter_render"]),
                format_table_row("Repaint render", series["repaint_render"]),
                format_table_row("Window present", series["window_present"]),
                format_table_row("Media inter-frame", series["media_inter_frame"]),
                format_table_row("Media summary avg-frame", series["media_summary_avg_frame"]),
                format_table_row(
                    "GStreamer sample pts-delta",
                    series["gst_video_sample_pts_delta"],
                ),
                format_table_row(
                    "GStreamer sample wall-delta",
                    series["gst_video_sample_wall_delta"],
                ),
                format_table_row(
                    "GStreamer summary pts-delta",
                    series["gst_video_summary_pts_delta"],
                ),
                format_table_row(
                    "GStreamer summary wall-delta",
                    series["gst_video_summary_wall_delta"],
                ),
                format_table_row(
                    "GStreamer summary avg-wall",
                    series["gst_video_summary_avg_wall"],
                ),
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
    require_yuv: bool,
    max_barrier_request_p95_ms: float | None,
    max_media_inter_frame_p95_ms: float | None,
    max_media_summary_avg_p95_ms: float | None,
    max_gst_video_summary_avg_wall_p95_ms: float | None,
    max_gst_video_summary_late_wall_gaps: int | None,
    max_wall_frame_overlaps: int | None,
    max_wall_frame_pending_nonzero: int | None,
) -> list[str]:
    counts = result["counts"]
    present = result["present_balance"]
    series = result["series"]
    failures = []

    if counts["panics"] > 0:
        failures.append(f"panic diagnostics present: {counts['panics']}")
    if counts["errors"] > 0:
        failures.append(f"error diagnostics present: {counts['errors']}")
    if counts["metadata_mismatched"] > 0:
        failures.append(f"metadata mismatches present: {counts['metadata_mismatched']}")
    if counts["media_frames_observed"] == 0:
        failures.append("no media frame diagnostics found")
    if counts["media_image_fanouts_observed"] == 0:
        failures.append("no media image fan-out diagnostics found")
    if require_yuv and (
        counts["media_yuv_i420_frames_observed"] + counts["media_yuv_nv12_frames_observed"]
    ) == 0:
        failures.append("no observed YUV media frames")
    if counts["raw_yuv_plane_locks_observed"] != counts["raw_yuv_plane_unlocks_observed"]:
        failures.append(
            "raw YUV plane lock/unlock imbalance: "
            f"{counts['raw_yuv_plane_locks_observed']} / "
            f"{counts['raw_yuv_plane_unlocks_observed']}"
        )
    if present["spread"] > max_present_spread:
        failures.append(
            f"present count spread {present['spread']} exceeds {max_present_spread}"
        )
    if max_barrier_request_p95_ms is not None:
        barrier_p95 = series["barrier_request_to_all_ready"]["p95_ms"]
        if barrier_p95 is not None and barrier_p95 > max_barrier_request_p95_ms:
            failures.append(
                f"barrier request-to-all-ready p95 {barrier_p95} ms exceeds "
                f"{max_barrier_request_p95_ms} ms"
            )
    if max_media_inter_frame_p95_ms is not None:
        media_p95 = series["media_inter_frame"]["p95_ms"]
        if media_p95 is not None and media_p95 > max_media_inter_frame_p95_ms:
            failures.append(
                f"media inter-frame p95 {media_p95} ms exceeds "
                f"{max_media_inter_frame_p95_ms} ms"
            )
    if max_media_summary_avg_p95_ms is not None:
        media_summary_p95 = series["media_summary_avg_frame"]["p95_ms"]
        if media_summary_p95 is not None and media_summary_p95 > max_media_summary_avg_p95_ms:
            failures.append(
                f"media summary avg-frame p95 {media_summary_p95} ms exceeds "
                f"{max_media_summary_avg_p95_ms} ms"
            )
    if max_gst_video_summary_avg_wall_p95_ms is not None:
        gst_summary_p95 = series["gst_video_summary_avg_wall"]["p95_ms"]
        if (
            gst_summary_p95 is not None and
            gst_summary_p95 > max_gst_video_summary_avg_wall_p95_ms
        ):
            failures.append(
                f"gstreamer video summary avg-wall p95 {gst_summary_p95} ms exceeds "
                f"{max_gst_video_summary_avg_wall_p95_ms} ms"
            )
    if (
        max_gst_video_summary_late_wall_gaps is not None and
        counts["gst_video_summary_late_wall_gaps"] >
        max_gst_video_summary_late_wall_gaps
    ):
        failures.append(
            f"gstreamer video summary late wall gaps "
            f"{counts['gst_video_summary_late_wall_gaps']} exceed "
            f"{max_gst_video_summary_late_wall_gaps}"
        )
    if (
        max_wall_frame_overlaps is not None
        and counts["wall_frame_overlaps"] > max_wall_frame_overlaps
    ):
        failures.append(
            f"wall frame overlaps {counts['wall_frame_overlaps']} exceed "
            f"{max_wall_frame_overlaps}"
        )
    if (
        max_wall_frame_pending_nonzero is not None
        and counts["wall_frame_pending_nonzero"] > max_wall_frame_pending_nonzero
    ):
        failures.append(
            f"wall frame pending-nonzero requests "
            f"{counts['wall_frame_pending_nonzero']} exceed "
            f"{max_wall_frame_pending_nonzero}"
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
    parser.add_argument(
        "--require-yuv",
        action="store_true",
        help="fail if no YUV media frames are observed",
    )
    parser.add_argument(
        "--max-barrier-request-p95-ms",
        type=float,
        default=None,
        help="fail if barrier request-to-all-ready p95 exceeds this value",
    )
    parser.add_argument(
        "--max-media-inter-frame-p95-ms",
        type=float,
        default=None,
        help="fail if media inter-frame p95 exceeds this value",
    )
    parser.add_argument(
        "--max-media-summary-avg-p95-ms",
        type=float,
        default=None,
        help="fail if media summary average frame p95 exceeds this value",
    )
    parser.add_argument(
        "--max-gst-video-summary-avg-wall-p95-ms",
        type=float,
        default=None,
        help="fail if GStreamer video summary average wall-frame p95 exceeds this value",
    )
    parser.add_argument(
        "--max-gst-video-summary-late-wall-gaps",
        type=int,
        default=None,
        help="fail if GStreamer video summary late wall gaps exceed this value",
    )
    parser.add_argument(
        "--max-wall-frame-overlaps",
        type=int,
        default=None,
        help="fail if wall frame overlap diagnostics exceed this count",
    )
    parser.add_argument(
        "--max-wall-frame-pending-nonzero",
        type=int,
        default=None,
        help="fail if wall frame requests with pending_before > 0 exceed this count",
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
                result,
                args.expected_gpu,
                args.max_present_spread,
                args.require_yuv,
                args.max_barrier_request_p95_ms,
                args.max_media_inter_frame_p95_ms,
                args.max_media_summary_avg_p95_ms,
                args.max_gst_video_summary_avg_wall_p95_ms,
                args.max_gst_video_summary_late_wall_gaps,
                args.max_wall_frame_overlaps,
                args.max_wall_frame_pending_nonzero,
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
