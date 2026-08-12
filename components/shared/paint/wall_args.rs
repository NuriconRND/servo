/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! 두 셸이 공유하는 wall 인자의 **의미**.
//!
//! 파서는 통일하지 않는다(비목표) — servoshell 은 bpaf, winit_wall 은 손으로 쓴 `match`
//! 루프다. 두 셸은 각자 방식으로 [`WallArgs`] 를 **채우기만** 하고, 검증과 해석은 여기
//! 한 곳에서 한다. 같은 플래그를 두 곳에서 따로 해석하던 것이 이미 갈라져 있었다:
//! servoshell 은 `--wall-layout` 없이 `--wall-tile-index` 를 주면 경고했고 winit_wall 은
//! 아예 검사하지 않았다.
//!
//! ## 경고를 오류로 올렸다 (동작 변경)
//!
//! 아래 두 경우는 **사용자가 요청한 것이 일어나지 않는** 상태다. 옛 servoshell 은 `warn!`
//! 로 넘겼는데, 이 프로젝트가 반복해서 데인 실패 형태가 정확히 그것이다 — 켰다고 믿는데
//! 안 켜진 상태(Task 3 의 죽은 `-DComp` 스위치, Task 6 의 차단 판단). 로그를 리다이렉트해야
//! 보이는 `warn!` 은 GUI 실행에서 사실상 보이지 않는다.
//!
//! - `--wall-tile-index N` 인데 `--wall-layout` 이 없다 → [`WallArgsError::TileIndexWithoutLayout`]
//! - `--wall-tile-index N` 과 `--wall-all-tiles` 를 함께 줬다 → [`WallArgsError::TileIndexWithAllTiles`]
//!
//! 저장소의 `etc/multigpu/*.ps1` 전량을 확인했다 — 이 조합을 넘기는 스크립트는 **없다**
//! (`run_kakao_map_wall.ps1` 은 if/else 로 배타적으로 준다). 그래서 이 격상이 기존 운용을
//! 깨뜨리지 않는다.
//!
//! ## `tile_index` 가 `Option<usize>` 인 이유
//!
//! 설계 문서 §8 의 스케치는 `usize` 였다. 그런데 그러면 **"주지 않았다" 와 "0 을 줬다" 를
//! 구분할 수 없다** — 위 두 규칙은 "네가 요청한 것이 무시된다" 를 알리는 것이라, 기본값
//! 0 이 요청으로 오인되면 `--wall-all-tiles` 단독 실행이 전부 오류가 된다. 채우는 쪽이
//! 플래그의 유무를 그대로 옮기게 했다.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::wall_layout::{WallLayout, WallLayoutError};

/// 두 셸이 각자 파싱해 채우는 wall 인자. 검증·해석은 [`WallArgs::validate`] /
/// [`WallArgs::resolve`] 한 곳에서 한다.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WallArgs {
    /// `--wall-layout <path>`. 없으면 wall 모드가 아니다(servoshell 은 평범한 창으로 뜬다).
    pub layout: Option<PathBuf>,
    /// `--wall-tile-index <n>`. **플래그를 준 경우에만** `Some` 이어야 한다 — 모듈 doc 참고.
    pub tile_index: Option<usize>,
    /// `--wall-all-tiles`. 타일마다 창을 하나씩 연다.
    pub all_tiles: bool,
}

#[derive(Debug)]
pub enum WallArgsError {
    /// `--wall-tile-index` 를 줬는데 `--wall-layout` 이 없다.
    TileIndexWithoutLayout,
    /// `--wall-tile-index` 와 `--wall-all-tiles` 를 함께 줬다. 후자가 타일마다 창을
    /// 만들므로 앞의 인덱스는 쓰일 자리가 없다.
    TileIndexWithAllTiles,
    /// `--wall-all-tiles` 를 줬는데 `--wall-layout` 이 없다.
    AllTilesWithoutLayout,
    /// 레이아웃 파일을 읽거나 검증하는 데 실패했다. 경로를 함께 담는다 — 어느 파일이
    /// 문제인지 없이 파싱 오류만 보면 추적이 한 단계 늘어난다.
    Layout(PathBuf, WallLayoutError),
}

impl fmt::Display for WallArgsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WallArgsError::TileIndexWithoutLayout => write!(
                f,
                "--wall-tile-index needs --wall-layout <path>; without a layout there are no tiles"
            ),
            WallArgsError::TileIndexWithAllTiles => write!(
                f,
                "--wall-tile-index cannot be combined with --wall-all-tiles; \
                 --wall-all-tiles opens one window per tile, so a single index has no meaning"
            ),
            WallArgsError::AllTilesWithoutLayout => write!(
                f,
                "--wall-all-tiles needs --wall-layout <path>; without a layout there are no tiles"
            ),
            WallArgsError::Layout(path, error) => {
                write!(f, "could not parse wall layout {}: {error}", path.display())
            },
        }
    }
}

impl std::error::Error for WallArgsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            WallArgsError::Layout(_, error) => Some(error),
            _ => None,
        }
    }
}

impl WallArgs {
    /// 파일을 읽지 않고 플래그 조합만 본다. [`Self::resolve`] 가 먼저 이것을 부르므로 셸이
    /// 따로 부를 필요는 없다 — 조합만 확인하고 싶을 때(테스트 등) 쓰라고 공개해 둔다.
    pub fn validate(&self) -> Result<(), WallArgsError> {
        if self.tile_index.is_some() && self.all_tiles {
            return Err(WallArgsError::TileIndexWithAllTiles);
        }
        if self.layout.is_none() {
            if self.tile_index.is_some() {
                return Err(WallArgsError::TileIndexWithoutLayout);
            }
            if self.all_tiles {
                return Err(WallArgsError::AllTilesWithoutLayout);
            }
        }
        Ok(())
    }

    /// 레이아웃이 지정돼 있으면 읽어서 검증까지 마친 것을 돌려준다. `--wall-layout` 이
    /// 없으면 `Ok(None)` — wall 모드가 아니라는 뜻이고, 그것을 오류로 볼지는 셸이 정한다
    /// (servoshell 은 평범한 창으로 뜨고 winit_wall 은 표출 전용이라 필수다).
    pub fn resolve(&self) -> Result<Option<WallLayout>, WallArgsError> {
        self.validate()?;
        let Some(path) = self.layout.as_deref() else {
            return Ok(None);
        };
        let layout = WallLayout::from_path(path)
            .map_err(|error| WallArgsError::Layout(path.to_path_buf(), error))?;
        // `--wall-all-tiles` 면 인덱스가 없으므로 범위 검사할 대상이 없다(validate 가 이미
        // 두 플래그의 공존을 막았다).
        if let Some(tile_index) = self.tile_index {
            layout
                .validate_tile_index(tile_index)
                .map_err(|error| WallArgsError::Layout(path.to_path_buf(), error))?;
        }
        Ok(Some(layout))
    }

    /// 이 실행이 실제로 그릴 타일 인덱스. `--wall-all-tiles` 는 창마다 자기 인덱스를 따로
    /// 쓰므로 여기서는 의미가 없고, 단일 타일 모드의 기본값은 0 이다.
    pub fn effective_tile_index(&self) -> usize {
        self.tile_index.unwrap_or(0)
    }
}

/// `--wall-layout` 인자를 [`PathBuf`] 로 받는 셸용 편의 생성자.
impl WallArgs {
    pub fn new(layout: Option<&Path>, tile_index: Option<usize>, all_tiles: bool) -> Self {
        Self {
            layout: layout.map(Path::to_path_buf),
            tile_index,
            all_tiles,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_index_without_a_layout_is_rejected() {
        let args = WallArgs::new(None, Some(2), false);
        assert!(matches!(
            args.validate(),
            Err(WallArgsError::TileIndexWithoutLayout)
        ));
    }

    #[test]
    fn all_tiles_makes_the_tile_index_meaningless() {
        let args = WallArgs::new(Some(Path::new("x.json")), Some(3), true);
        assert!(matches!(
            args.validate(),
            Err(WallArgsError::TileIndexWithAllTiles)
        ));
    }

    #[test]
    fn all_tiles_without_a_layout_is_rejected() {
        let args = WallArgs::new(None, None, true);
        assert!(matches!(
            args.validate(),
            Err(WallArgsError::AllTilesWithoutLayout)
        ));
    }

    #[test]
    fn the_conflict_check_does_not_fire_on_a_bare_all_tiles_run() {
        // ★이것이 tile_index 를 Option 으로 둔 이유★ — usize + 기본값 0 이었다면
        // `--wall-all-tiles` 단독 실행이 "인덱스 0 을 줬다" 로 읽혀 전부 오류가 됐다.
        let args = WallArgs::new(Some(Path::new("x.json")), None, true);
        assert!(args.validate().is_ok());
    }

    #[test]
    fn no_wall_flags_at_all_is_a_normal_browser_run() {
        let args = WallArgs::default();
        assert!(args.validate().is_ok());
        assert!(args.resolve().expect("검증 통과").is_none());
    }

    #[test]
    fn a_missing_layout_file_names_the_path_it_tried() {
        // 경로 없이 파싱 오류만 보면 어느 파일이 문제인지 다시 찾아야 한다.
        let args = WallArgs::new(Some(Path::new("no_such_wall_layout.json")), None, false);
        let error = args.resolve().expect_err("없는 파일이다");
        assert!(matches!(error, WallArgsError::Layout(..)));
        assert!(
            error.to_string().contains("no_such_wall_layout.json"),
            "{error}"
        );
    }

    #[test]
    fn effective_tile_index_defaults_to_zero() {
        assert_eq!(WallArgs::default().effective_tile_index(), 0);
        assert_eq!(
            WallArgs::new(None, Some(4), false).effective_tile_index(),
            4
        );
    }
}
