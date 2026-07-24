/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::fmt;
use std::fs;
use std::path::Path;

use euclid::{Point2D, Rect, Scale, Size2D, Vector2D};
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
                log::warn!(
                    "wall tile {index}: 'monitor' is deprecated; use 'display' (spatial index, \
                     top-left = 0)"
                );
                monitor
            },
        };
        if tile.get("gpu").is_some() {
            log::warn!(
                "wall tile {index}: 'gpu' is ignored; the GPU is auto-assigned from the adapter \
                 that drives the chosen display"
            );
        }
        let rect = get_rect(tile, "rect")?;
        validate_tile_rect(index, rect, virtual_viewport)?;
        parsed_tiles.push(WallTile { display, rect });
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
    fn accepts_legacy_monitor_alias_and_ignores_gpu() {
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
        assert_eq!(layout.tiles[1].display, 0);
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
}
