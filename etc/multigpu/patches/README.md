# mozangle ANGLE 패치 — 멀티 GPU 월 연산 분산

## 이 패치가 하는 일
`mozangle-0.5.5-angle-luid-display-cache.patch` 는 ANGLE의 EGL display 캐시 키에
**D3D adapter LUID** 를 추가한다.

- 문제: ANGLE의 `ANGLEPlatformDisplay` 캐시 키는 `nativeDisplay / powerPreference /
  platformType / deviceIdHigh / deviceIdLow` 만 사용하고 **LUID는 빠져 있다.**
  surfman은 GPU를 LUID로 선택하므로, GPU마다 LUID만 다른 요청이 **같은 캐시 키로 충돌**해
  처음 만들어진 (보통 GPU0) display 가 모든 어댑터에 재사용된다 → 모든 렌더가 GPU0 집중.
  (동일 모델 GPU 2개는 deviceId로도 구분 불가.)
- 수정: `Display.cpp` 4곳에 `luidHigh/luidLow` 추가
  (구조체 필드 / 7-인자 생성자 / `tie()` / `GetDisplayFromNativeDisplay` 키 / `~Display` 캐시 erase 키).
- 결과: LUID별로 별도 display 가 생기고 `callD3D11CreateDevice` 가 각 LUID를 올바른 어댑터에 매칭
  → 각 월 타일이 자기 모니터의 물리 GPU에서 렌더/합성.
  검증: nvidia-smi 에서 servoshell 이 **양 GPU 모두에 compute-app 등록**, 메모리/사용률 균형.

## 왜 직접 커밋이 안 되나
mozangle 은 crates.io 의존성이라 빌드가 **cargo 레지스트리 캐시**(레포 밖)의 소스를 컴파일한다
(`~/.cargo/registry/...` 또는 `CARGO_HOME=.servo\cargo-home` 일 때 `.servo/...`).
`.servo` 는 `.gitignore` 됐고 `~/.cargo` 는 사용자 홈이라 **이 레포가 추적할 수 없다.**
대조적으로 surfman 은 `third_party/surfman` 에 벤더링 + `[patch.crates-io]` 라 커밋된다.

## 임시 적용 (현재 머신에서 동작시키기)
```powershell
# 패치만 적용(찾은 모든 mozangle 트리에, 멱등)
etc\multigpu\patches\apply_mozangle_angle_luid.ps1
# 적용 + ANGLE 강제 리빌드 + DLL 복사
etc\multigpu\patches\apply_mozangle_angle_luid.ps1 -Rebuild
```
주의: ANGLE `.cpp` 는 cc 가 rerun-if-changed 를 안 내므로, 패치 후 반드시
`mozangle-*` 빌드 산출물을 지우고 리빌드해야 반영된다(스크립트의 `-Rebuild` 가 처리).

## 영구/커밋 가능한 형태 (권장)
mozangle 을 포크해 이 패치를 적용한 뒤 `Cargo.toml` 에 한 줄:
```toml
[patch.crates-io]
mozangle = { git = "https://github.com/<your-fork>/mozangle", branch = "angle-luid-display-cache" }
```
→ 변경이 레포(Cargo.toml + 포크)에 들어가 커밋·재현·CI 모두 가능.
(mozangle 은 ANGLE 전체 C++ 소스 수백 MB 를 번들하므로 `third_party/` 벤더링은 비현실적 → 포크가 정답.)

## 관련
- 전체 상태/인계: `etc/multigpu/WEBGL_WALL_STATUS.md`
- surfman 측(타일별 격리 D3D11 디바이스 + cross-GPU surface): 커밋 35b4c2ed7
