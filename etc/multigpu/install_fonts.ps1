# install_fonts.ps1 — 표출 페이지용 한글 폰트 오프라인 설치 (사용자 단위, 관리자/인터넷/재부팅 불필요)
#
# 배경 (2026-07-18 실측, Servo 빌드 기준):
#  - Servo는 @font-face 웹폰트를 로드하지 않는다 (전 형태 실측 실패).
#  - 같은 패밀리 내 font-weight 선택도 동작하지 않는다 → Bold는 "전용 패밀리"로 분리 설치.
#  - 설치된 "Malgun Gothic"은 이름 매칭에 실패하지만 "Noto Sans KR"/"Noto Sans CJK KR"은 매칭된다.
#  → 표출 페이지는 본문="Noto Sans KR"(Regular), 굵은 요소="Noto Sans CJK KR"(Bold)로 지정되어 있다.
#
# 설치물 (스크립트 옆 fonts\ 또는 tests\fonts\ 에서 탐색):
#  - NotoSansKR-Regular.otf  → 패밀리 "Noto Sans KR"
#  - NotoSansCJKkr-Bold.otf  → 패밀리 "Noto Sans CJK KR"
# 라이선스: SIL OFL 1.1 (동봉 LICENSE-OFL.txt)

$ErrorActionPreference = 'Stop'

$candidates = @(
    (Join-Path $PSScriptRoot 'fonts'),
    (Join-Path $PSScriptRoot 'tests\fonts'),
    (Join-Path $PSScriptRoot '..\..\tests\fonts')
)
$srcDir = $candidates | Where-Object { Test-Path (Join-Path $_ 'NotoSansKR-Regular.otf') } | Select-Object -First 1
if (-not $srcDir) {
    Write-Host "ERROR: font source dir not found (looked in: $($candidates -join '; '))"
    exit 1
}

$fonts = @(
    @{ File = 'NotoSansKR-Regular.otf';  RegName = 'Noto Sans KR (OpenType)' },
    @{ File = 'NotoSansCJKkr-Bold.otf';  RegName = 'Noto Sans CJK KR Bold (OpenType)' }
)

$dstDir = Join-Path $env:LOCALAPPDATA 'Microsoft\Windows\Fonts'
New-Item -ItemType Directory -Force $dstDir | Out-Null
$regPath = 'HKCU:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Fonts'
if (-not (Test-Path $regPath)) { New-Item -Path $regPath -Force | Out-Null }

Add-Type -Name GdiFont -Namespace Win -MemberDefinition '[DllImport("gdi32.dll", CharSet=CharSet.Unicode)] public static extern int AddFontResourceW(string lpFileName);'

foreach ($f in $fonts) {
    $src = Join-Path $srcDir $f.File
    $dst = Join-Path $dstDir $f.File
    if (-not (Test-Path $src)) { Write-Host "SKIP (missing source): $($f.File)"; continue }
    Copy-Item $src $dst -Force
    New-ItemProperty -Path $regPath -Name $f.RegName -Value $dst -PropertyType String -Force | Out-Null
    [Win.GdiFont]::AddFontResourceW($dst) | Out-Null
    Write-Host "installed: $($f.File) -> $($f.RegName)"
}
Write-Host 'done. (servoshell 새 프로세스부터 적용 — 재부팅 불필요)'
