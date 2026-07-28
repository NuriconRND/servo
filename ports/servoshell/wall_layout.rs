/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::fmt;
use std::fs;
use std::path::Path;

use euclid::{Point2D, Rect, Scale, SideOffsets2D, Size2D, Vector2D};
use serde_json::Value;
use servo::{CSSPixel, DeviceIndependentPixel, DeviceIntRect, DevicePixel};

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct WallLayout {
    pub(crate) virtual_viewport: Size2D<u32, DeviceIndependentPixel>,
    pub(crate) tiles: Vec<WallTile>,
    pub(crate) overlap_px: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct WallTile {
    /// Spatial display index (top-left = 0, left→right then top→bottom). Resolved at
    /// window-creation time against the DXGI display topology; the GPU that drives that
    /// display is auto-assigned. The legacy `monitor` field is accepted as an alias.
    pub(crate) display: usize,
    pub(crate) rect: Rect<i32, DeviceIndependentPixel>,
    /// Explicit GPU adapter override. When present, this wins over the auto-assigned adapter
    /// that drives `display` (e.g. to deliberately render a tile on a GPU that does NOT drive
    /// its display, for cross-GPU testing). `None` keeps the default auto-GPU behavior.
    pub(crate) gpu_override: Option<usize>,
}

#[derive(Debug)]
pub(crate) enum WallLayoutError {
    Io(std::io::Error),
    Json(serde_json::Error),
    Invalid(String),
}

impl WallLayout {
    pub(crate) fn from_path(path: &Path) -> Result<Self, WallLayoutError> {
        let text = fs::read_to_string(path).map_err(WallLayoutError::Io)?;
        Self::from_json_str(&text)
    }

    pub(crate) fn from_json_str(text: &str) -> Result<Self, WallLayoutError> {
        let value: Value = serde_json::from_str(text).map_err(WallLayoutError::Json)?;
        let virtual_viewport = parse_virtual_viewport(&value)?;
        let tiles = parse_tiles(&value, virtual_viewport)?;
        let overlap_px = get_optional_u32(&value, "overlapPx")?.unwrap_or(0);

        Ok(Self {
            virtual_viewport,
            tiles,
            overlap_px,
        })
    }

    pub(crate) fn validate_tile_index(&self, tile_index: usize) -> Result<(), WallLayoutError> {
        if tile_index >= self.tiles.len() {
            return Err(WallLayoutError::Invalid(format!(
                "wall tile index {tile_index} is out of range; layout has {} tile(s)",
                self.tiles.len()
            )));
        }
        Ok(())
    }

    pub(crate) fn virtual_viewport_css_size(&self) -> Size2D<f32, CSSPixel> {
        Size2D::new(
            self.virtual_viewport.width as f32,
            self.virtual_viewport.height as f32,
        )
    }

    pub(crate) fn tile_origin_device_vector(
        &self,
        tile_index: usize,
        hidpi_scale_factor: Scale<f32, DeviceIndependentPixel, DevicePixel>,
    ) -> Vector2D<f32, DevicePixel> {
        let Some(tile) = self.tiles.get(tile_index) else {
            return Vector2D::zero();
        };
        Vector2D::new(
            tile.rect.origin.x as f32 * hidpi_scale_factor.get(),
            tile.rect.origin.y as f32 * hidpi_scale_factor.get(),
        )
    }

    pub(crate) fn virtual_viewport_device_size(
        &self,
        hidpi_scale_factor: Scale<f32, DeviceIndependentPixel, DevicePixel>,
    ) -> Size2D<i32, DevicePixel> {
        (self.virtual_viewport.to_f32() * hidpi_scale_factor).to_i32()
    }

    pub(crate) fn tile_device_rect(
        &self,
        tile_index: usize,
        hidpi_scale_factor: Scale<f32, DeviceIndependentPixel, DevicePixel>,
    ) -> Option<DeviceIntRect> {
        let tile = self.tiles.get(tile_index)?;
        Some(rect_to_device_rect(tile.rect, hidpi_scale_factor))
    }

    pub(crate) fn tile_render_rect(
        &self,
        tile_index: usize,
    ) -> Option<Rect<i32, DeviceIndependentPixel>> {
        let tile = self.tiles.get(tile_index)?;
        let overlap = i32::try_from(self.overlap_px).unwrap_or(i32::MAX);
        let min_x = tile.rect.origin.x.saturating_sub(overlap).max(0);
        let min_y = tile.rect.origin.y.saturating_sub(overlap).max(0);
        let max_x = tile
            .rect
            .max_x()
            .saturating_add(overlap)
            .min(self.virtual_viewport.width as i32);
        let max_y = tile
            .rect
            .max_y()
            .saturating_add(overlap)
            .min(self.virtual_viewport.height as i32);

        Some(Rect::new(
            Point2D::new(min_x, min_y),
            Size2D::new(max_x - min_x, max_y - min_y),
        ))
    }

    pub(crate) fn tile_render_device_rect(
        &self,
        tile_index: usize,
        hidpi_scale_factor: Scale<f32, DeviceIndependentPixel, DevicePixel>,
    ) -> Option<DeviceIntRect> {
        self.tile_render_rect(tile_index)
            .map(|rect| rect_to_device_rect(rect, hidpi_scale_factor))
    }

    /// 이 타일의 가드밴드 여백 — 오버랩 확장된 render rect 대비 visible rect의 각 변 간격.
    /// device px, **top-left 기준**. 표출 경로가 렌더 서피스에서 잘라내야 할 양이다.
    ///
    /// 소비자마다 y 규약이 다르다: GL blit(프레임버퍼 bottom-left 원점)은 src y 원점에
    /// `bottom`을, DComp(top-left 원점)는 root visual 오프셋에 `-top`을 쓴다
    /// (§`RenderingContext::present_inset`).
    pub(crate) fn tile_render_insets(
        &self,
        tile_index: usize,
        hidpi_scale_factor: Scale<f32, DeviceIndependentPixel, DevicePixel>,
    ) -> Option<SideOffsets2D<i32, DevicePixel>> {
        let visible = self.tile_device_rect(tile_index, hidpi_scale_factor)?;
        let render = self.tile_render_device_rect(tile_index, hidpi_scale_factor)?;
        Some(SideOffsets2D::new(
            (visible.min.y - render.min.y).max(0),
            (render.max.x - visible.max.x).max(0),
            (render.max.y - visible.max.y).max(0),
            (visible.min.x - render.min.x).max(0),
        ))
    }
}

fn rect_to_device_rect(
    rect: Rect<i32, DeviceIndependentPixel>,
    hidpi_scale_factor: Scale<f32, DeviceIndependentPixel, DevicePixel>,
) -> DeviceIntRect {
    let origin = Point2D::new(
        (rect.origin.x as f32 * hidpi_scale_factor.get()).round() as i32,
        (rect.origin.y as f32 * hidpi_scale_factor.get()).round() as i32,
    );
    let size = (rect.size.to_f32() * hidpi_scale_factor).to_i32();
    DeviceIntRect::from_origin_and_size(origin, size)
}

impl fmt::Display for WallLayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WallLayoutError::Io(error) => write!(f, "{error}"),
            WallLayoutError::Json(error) => write!(f, "{error}"),
            WallLayoutError::Invalid(message) => write!(f, "{message}"),
        }
    }
}

fn parse_virtual_viewport(
    value: &Value,
) -> Result<Size2D<u32, DeviceIndependentPixel>, WallLayoutError> {
    let viewport = get_object(value, "virtualViewport")?;
    let width = get_positive_u32(viewport, "width")?;
    let height = get_positive_u32(viewport, "height")?;
    Ok(Size2D::new(width, height))
}

fn parse_tiles(
    value: &Value,
    virtual_viewport: Size2D<u32, DeviceIndependentPixel>,
) -> Result<Vec<WallTile>, WallLayoutError> {
    let tiles = value
        .get("tiles")
        .and_then(Value::as_array)
        .ok_or_else(|| WallLayoutError::Invalid("tiles must be an array".to_string()))?;
    if tiles.is_empty() {
        return Err(WallLayoutError::Invalid(
            "tiles must contain at least one tile".to_string(),
        ));
    }

    let mut parsed_tiles = Vec::with_capacity(tiles.len());
    for (index, tile) in tiles.iter().enumerate() {
        let display = match get_usize(tile, "display") {
            Ok(display) => display,
            Err(_) => {
                let monitor = get_usize(tile, "monitor").map_err(|_| {
                    WallLayoutError::Invalid(format!(
                        "tile {index} must have a 'display' (spatial index) field"
                    ))
                })?;
                // NOTE: layout parsing runs during CLI argument parsing, before
                // `servo.setup_logging()` is called (see app.rs); any `log::` call here is
                // silently dropped. Use `eprintln!` so this warning is actually visible.
                eprintln!(
                    "wall layout: wall tile {index}: 'monitor' is deprecated; use 'display' \
                     (spatial index, top-left = 0)"
                );
                monitor
            },
        };
        let gpu_override = if tile.get("gpu").is_some() {
            let gpu = get_usize(tile, "gpu")?;
            // See NOTE above: `eprintln!` because logging isn't initialized yet at parse time.
            eprintln!(
                "wall layout: wall tile {index}: 'gpu' overrides the auto-assigned adapter \
                 (auto-GPU picks the adapter that drives display {display})"
            );
            Some(gpu)
        } else {
            None
        };
        let rect = get_rect(tile, "rect")?;
        validate_tile_rect(index, rect, virtual_viewport)?;
        parsed_tiles.push(WallTile {
            display,
            rect,
            gpu_override,
        });
    }
    Ok(parsed_tiles)
}

fn validate_tile_rect(
    index: usize,
    rect: Rect<i32, DeviceIndependentPixel>,
    virtual_viewport: Size2D<u32, DeviceIndependentPixel>,
) -> Result<(), WallLayoutError> {
    if rect.size.width <= 0 || rect.size.height <= 0 {
        return Err(WallLayoutError::Invalid(format!(
            "tile {index} rect width and height must be positive"
        )));
    }

    if rect.origin.x < 0
        || rect.origin.y < 0
        || rect.max_x() > virtual_viewport.width as i32
        || rect.max_y() > virtual_viewport.height as i32
    {
        return Err(WallLayoutError::Invalid(format!(
            "tile {index} rect [{}, {}, {}, {}] exceeds virtualViewport",
            rect.origin.x, rect.origin.y, rect.size.width, rect.size.height
        )));
    }

    Ok(())
}

fn get_object<'a>(
    value: &'a Value,
    key: &str,
) -> Result<&'a serde_json::Map<String, Value>, WallLayoutError> {
    value
        .get(key)
        .and_then(Value::as_object)
        .ok_or_else(|| WallLayoutError::Invalid(format!("{key} must be an object")))
}

fn get_positive_u32(
    value: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<u32, WallLayoutError> {
    let number = value
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| WallLayoutError::Invalid(format!("{key} must be a positive integer")))?;
    if number == 0 || number > u32::MAX as u64 {
        return Err(WallLayoutError::Invalid(format!(
            "{key} must be a positive 32-bit integer"
        )));
    }
    Ok(number as u32)
}

fn get_optional_u32(value: &Value, key: &str) -> Result<Option<u32>, WallLayoutError> {
    let Some(number) = value.get(key) else {
        return Ok(None);
    };
    let Some(number) = number.as_u64() else {
        return Err(WallLayoutError::Invalid(format!(
            "{key} must be a non-negative integer"
        )));
    };
    if number > u32::MAX as u64 {
        return Err(WallLayoutError::Invalid(format!(
            "{key} must be a 32-bit integer"
        )));
    }
    Ok(Some(number as u32))
}

fn get_usize(value: &Value, key: &str) -> Result<usize, WallLayoutError> {
    let number = value
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| WallLayoutError::Invalid(format!("{key} must be a non-negative integer")))?;
    usize::try_from(number)
        .map_err(|_| WallLayoutError::Invalid(format!("{key} is too large for this platform")))
}

fn get_rect(
    value: &Value,
    key: &str,
) -> Result<Rect<i32, DeviceIndependentPixel>, WallLayoutError> {
    let rect = value
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| WallLayoutError::Invalid(format!("{key} must be [x, y, width, height]")))?;
    if rect.len() != 4 {
        return Err(WallLayoutError::Invalid(format!(
            "{key} must contain exactly four integers"
        )));
    }

    let mut values = [0; 4];
    for (index, value) in rect.iter().enumerate() {
        let Some(number) = value.as_i64() else {
            return Err(WallLayoutError::Invalid(format!(
                "{key}[{index}] must be an integer"
            )));
        };
        values[index] = i32::try_from(number)
            .map_err(|_| WallLayoutError::Invalid(format!("{key}[{index}] is out of i32 range")))?;
    }

    Ok(Rect::new(
        Point2D::new(values[0], values[1]),
        Size2D::new(values[2], values[3]),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_wall_layout() {
        let layout = WallLayout::from_json_str(
            r#"{
                "virtualViewport": { "width": 7680, "height": 4320 },
                "tiles": [
                    { "display": 0, "rect": [0, 0, 3840, 2160] },
                    { "display": 1, "rect": [3840, 0, 3840, 2160] }
                ],
                "overlapPx": 32
            }"#,
        )
        .expect("valid layout should parse");

        assert_eq!(layout.virtual_viewport, Size2D::new(7680, 4320));
        assert_eq!(layout.tiles.len(), 2);
        assert_eq!(layout.tiles[0].display, 0);
        assert_eq!(layout.tiles[1].display, 1);
        assert_eq!(layout.tiles[1].rect.origin, Point2D::new(3840, 0));
        assert_eq!(layout.overlap_px, 32);
    }

    #[test]
    fn rejects_out_of_bounds_tile() {
        let error = WallLayout::from_json_str(
            r#"{
                "virtualViewport": { "width": 100, "height": 100 },
                "tiles": [
                    { "display": 0, "rect": [90, 0, 20, 20] }
                ]
            }"#,
        )
        .expect_err("out-of-bounds tile should fail");

        assert!(error.to_string().contains("exceeds virtualViewport"));
    }

    #[test]
    fn calculates_overlap_render_rect_clamped_to_virtual_viewport() {
        let layout = WallLayout::from_json_str(
            r#"{
                "virtualViewport": { "width": 5760, "height": 1080 },
                "tiles": [
                    { "display": 0, "rect": [0, 0, 1920, 1080] },
                    { "display": 1, "rect": [1920, 0, 1920, 1080] },
                    { "display": 2, "rect": [3840, 0, 1920, 1080] }
                ],
                "overlapPx": 32
            }"#,
        )
        .expect("valid layout should parse");

        assert_eq!(
            layout.tile_render_rect(0).unwrap(),
            Rect::new(Point2D::new(0, 0), Size2D::new(1952, 1080))
        );
        assert_eq!(
            layout.tile_render_rect(1).unwrap(),
            Rect::new(Point2D::new(1888, 0), Size2D::new(1984, 1080))
        );
        assert_eq!(
            layout.tile_render_rect(2).unwrap(),
            Rect::new(Point2D::new(3808, 0), Size2D::new(1952, 1080))
        );
    }

    #[test]
    fn accepts_legacy_monitor_alias_and_honors_gpu_override() {
        let layout = WallLayout::from_json_str(
            r#"{
                "virtualViewport": { "width": 3840, "height": 1080 },
                "tiles": [
                    { "monitor": 2, "gpu": 7, "rect": [0, 0, 1920, 1080] },
                    { "monitor": 0, "gpu": 3, "rect": [1920, 0, 1920, 1080] }
                ]
            }"#,
        )
        .expect("legacy monitor+gpu layout should still parse");

        assert_eq!(layout.tiles[0].display, 2);
        assert_eq!(layout.tiles[0].gpu_override, Some(7));
        assert_eq!(layout.tiles[1].display, 0);
        assert_eq!(layout.tiles[1].gpu_override, Some(3));
    }

    #[test]
    fn display_schema_tile_without_gpu_has_no_override() {
        let layout = WallLayout::from_json_str(
            r#"{
                "virtualViewport": { "width": 3840, "height": 1080 },
                "tiles": [
                    { "display": 0, "rect": [0, 0, 1920, 1080] },
                    { "display": 1, "rect": [1920, 0, 1920, 1080] }
                ]
            }"#,
        )
        .expect("display-schema layout should parse");

        assert_eq!(layout.tiles[0].gpu_override, None);
        assert_eq!(layout.tiles[1].gpu_override, None);
    }

    #[test]
    fn rejects_tile_without_display_or_monitor() {
        let error = WallLayout::from_json_str(
            r#"{
                "virtualViewport": { "width": 3840, "height": 1080 },
                "tiles": [ { "rect": [0, 0, 1920, 1080] } ]
            }"#,
        )
        .expect_err("tile without display/monitor should fail");
        assert!(error.to_string().contains("display"));
    }

    /// 스케일 1.0에서 타일의 가드밴드 여백을 뽑는 테스트 헬퍼.
    fn insets(layout: &WallLayout, tile_index: usize) -> SideOffsets2D<i32, DevicePixel> {
        layout
            .tile_render_insets(tile_index, Scale::new(1.0))
            .expect("tile index should be valid")
    }

    #[test]
    fn tile_render_insets_2x2() {
        let layout = WallLayout::from_json_str(
            r#"{
                "virtualViewport": { "width": 3840, "height": 2160 },
                "tiles": [
                    { "display": 0, "rect": [0, 0, 1920, 1080] },
                    { "display": 1, "rect": [1920, 0, 1920, 1080] },
                    { "display": 2, "rect": [0, 1080, 1920, 1080] },
                    { "display": 3, "rect": [1920, 1080, 1920, 1080] }
                ],
                "overlapPx": 32
            }"#,
        )
        .expect("valid layout should parse");

        // SideOffsets2D::new(top, right, bottom, left).
        // 가드밴드는 항상 가상 뷰포트 '안쪽' 변에만 붙는다(바깥 변은 클램프).
        assert_eq!(insets(&layout, 0), SideOffsets2D::new(0, 32, 32, 0));
        assert_eq!(insets(&layout, 1), SideOffsets2D::new(0, 0, 32, 32));
        assert_eq!(insets(&layout, 2), SideOffsets2D::new(32, 32, 0, 0));
        assert_eq!(insets(&layout, 3), SideOffsets2D::new(32, 0, 0, 32));
        // 필드명 assert: SideOffsets2D::new(top, right, bottom, left) 인자 순서가 뒤바뀌어도
        // 대칭 케이스에서는 전체 비교가 우연히 통과할 수 있으므로, 필드를 직접 짚어 확인한다.
        assert_eq!(insets(&layout, 1).left, 32);
    }

    #[test]
    fn tile_render_insets_1x2_vertical() {
        let layout = WallLayout::from_json_str(
            r#"{
                "virtualViewport": { "width": 1920, "height": 2160 },
                "tiles": [
                    { "display": 0, "rect": [0, 0, 1920, 1080] },
                    { "display": 1, "rect": [0, 1080, 1920, 1080] }
                ],
                "overlapPx": 32
            }"#,
        )
        .expect("valid layout should parse");

        assert_eq!(insets(&layout, 0), SideOffsets2D::new(0, 0, 32, 0));
        assert_eq!(insets(&layout, 1), SideOffsets2D::new(32, 0, 0, 0));
        // 필드명 assert(위 2x2 테스트와 동일한 이유).
        assert_eq!(insets(&layout, 0).bottom, 32);
    }

    #[test]
    fn tile_render_insets_zero_without_overlap() {
        let layout = WallLayout::from_json_str(
            r#"{
                "virtualViewport": { "width": 3840, "height": 1080 },
                "tiles": [
                    { "display": 0, "rect": [0, 0, 1920, 1080] },
                    { "display": 1, "rect": [1920, 0, 1920, 1080] }
                ]
            }"#,
        )
        .expect("valid layout should parse");

        assert_eq!(insets(&layout, 0), SideOffsets2D::zero());
        assert_eq!(insets(&layout, 1), SideOffsets2D::zero());
    }

    #[test]
    fn tile_render_insets_clamped_when_overlap_exceeds_viewport() {
        let layout = WallLayout::from_json_str(
            r#"{
                "virtualViewport": { "width": 200, "height": 100 },
                "tiles": [
                    { "display": 0, "rect": [0, 0, 100, 100] },
                    { "display": 1, "rect": [100, 0, 100, 100] }
                ],
                "overlapPx": 500
            }"#,
        )
        .expect("valid layout should parse");

        // 오버랩이 뷰포트를 넘으면 render rect가 뷰포트 전체로 클램프된다.
        assert_eq!(insets(&layout, 0), SideOffsets2D::new(0, 100, 0, 0));
        assert_eq!(insets(&layout, 1), SideOffsets2D::new(0, 0, 0, 100));
    }

    #[test]
    fn tile_render_insets_none_for_out_of_range_tile() {
        let layout = WallLayout::from_json_str(
            r#"{
                "virtualViewport": { "width": 1920, "height": 1080 },
                "tiles": [ { "display": 0, "rect": [0, 0, 1920, 1080] } ]
            }"#,
        )
        .expect("valid layout should parse");

        assert!(layout.tile_render_insets(1, Scale::new(1.0)).is_none());
    }

    #[test]
    fn tile_render_insets_3x1_middle_tile_has_guard_bands_on_both_sides() {
        // 개발 머신의 `local_3x1` 실제 지오메트리(가로 3분할, 5760x1080). 기존 2x2/1x2
        // 테스트는 타일마다 가로·세로 각 한쪽 변에만 가드밴드가 붙는 형태만 다뤘는데,
        // 가운데 타일은 좌우 양쪽 모두에 가드밴드가 붙는 모양이라 별도로 검증한다
        // (이 layout의 tile_render_rect 값은 `calculates_overlap_render_rect_clamped_to_virtual_viewport`
        // 에서 이미 검증됨).
        let layout = WallLayout::from_json_str(
            r#"{
                "virtualViewport": { "width": 5760, "height": 1080 },
                "tiles": [
                    { "display": 0, "rect": [0, 0, 1920, 1080] },
                    { "display": 1, "rect": [1920, 0, 1920, 1080] },
                    { "display": 2, "rect": [3840, 0, 1920, 1080] }
                ],
                "overlapPx": 32
            }"#,
        )
        .expect("valid layout should parse");

        assert_eq!(insets(&layout, 0), SideOffsets2D::new(0, 32, 0, 0));
        // 가운데 타일: 좌우 양쪽 모두 가드밴드(overlapPx만큼), 위아래는 0.
        assert_eq!(insets(&layout, 1), SideOffsets2D::new(0, 32, 0, 32));
        assert_eq!(insets(&layout, 2), SideOffsets2D::new(0, 0, 0, 32));
        // 필드명 assert.
        assert_eq!(insets(&layout, 1).left, 32);
        assert_eq!(insets(&layout, 1).right, 32);
    }

    #[test]
    fn tile_render_insets_fractional_dpi_pins_current_rounding_behavior() {
        // `rect_to_device_rect`는 origin은 반올림(`.round()`)하고 size는 절삭(`as i32`
        // truncation)한다. scale=1.0에서는 두 연산이 정수 입력에 대해 항상 일치하지만,
        // 분수 DPI(예: 1.5)에서는 visible rect와 render rect가 *서로 다른 DIP 좌표*에서
        // 독립적으로 반올림/절삭되므로 어긋날 수 있다 — 이 테스트는 그 실제 동작을
        // 손으로 계산해 고정(pin)한다. 그린으로 만들려는 목적이 아니라 현재 동작을
        // 있는 그대로 박제하는 목적이다.
        //
        // 레이아웃: 가로 1923(=641*3, 홀수 폭) 3분할, overlapPx=11, scale=1.5.
        //
        // 손계산(모든 곱이 f32에서 정확히 표현되는 값들이라 반올림 오차 없음):
        //   tile0 visible DIP=[0,641]   -> device origin=round(0)=0,   width=trunc(961.5)=961 -> [0,961]
        //   tile0 render  DIP=[0,652]   -> device origin=round(0)=0,   width=trunc(978.0)=978 -> [0,978]
        //     => left=0-0=0, right=978-961=17
        //   tile1 visible DIP=[641,1282]-> device origin=round(961.5)=962, width=trunc(961.5)=961 -> [962,1923]
        //   tile1 render  DIP=[630,1293]-> device origin=round(945.0)=945, width=trunc(994.5)=994 -> [945,1939]
        //     => left=962-945=17, right=1939-1923=16
        //   tile2 visible DIP=[1282,1923]->device origin=round(1923.0)=1923, width=trunc(961.5)=961 -> [1923,2884]
        //   tile2 render  DIP=[1271,1923]->device origin=round(1906.5)=1907, width=trunc(978.0)=978 -> [1907,2885]
        //     => left=1923-1907=16, right=2885-2884=1
        //
        // 주목할 점(비일관성): tile2는 뷰포트 오른쪽 끝 타일이라 visible과 render 양쪽 모두
        // DIP 상에서 우측 경계가 뷰포트 끝(1923)에 정확히 클램프되어 "논리적으로는" 우측
        // 가드밴드가 0이어야 할 것 같지만, 각 rect가 origin+size를 독립적으로
        // 반올림/절삭하기 때문에 실제로는 `right=1`이 나온다(1923*1.5=2884.5라는 반쪽
        // 픽셀이 visible 쪽에서는 내림, render 쪽에서는 origin 반올림 때문에 다르게
        // 흡수됨). `webview_rendering_size`가 이 insets를 별도로 계산된 visible_size에
        // 더해 오프스크린 서피스 크기를 정하므로(gui.rs), 분수 DPI 월 타일에서는 실제로
        // 1px 단위의 크기 불일치가 생길 수 있다는 뜻이다.
        let layout = WallLayout::from_json_str(
            r#"{
                "virtualViewport": { "width": 1923, "height": 100 },
                "tiles": [
                    { "display": 0, "rect": [0, 0, 641, 100] },
                    { "display": 1, "rect": [641, 0, 641, 100] },
                    { "display": 2, "rect": [1282, 0, 641, 100] }
                ],
                "overlapPx": 11
            }"#,
        )
        .expect("valid layout should parse");

        let scale = Scale::new(1.5);
        let insets_at = |tile_index: usize| {
            layout
                .tile_render_insets(tile_index, scale)
                .expect("tile index should be valid")
        };

        assert_eq!(insets_at(0), SideOffsets2D::new(0, 17, 0, 0));
        assert_eq!(insets_at(1), SideOffsets2D::new(0, 16, 0, 17));
        assert_eq!(insets_at(2), SideOffsets2D::new(0, 1, 0, 16));
    }
}
