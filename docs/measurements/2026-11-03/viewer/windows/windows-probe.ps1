$ErrorActionPreference = 'Stop'
Set-Location C:\Users\Daniel\dev\latch-gfn-c
$env:RUST_LOG = 'latch_stream=info'
function RunViewer($arguments, $label) {
  $p = Start-Process -FilePath "$PWD\target\release\examples\present.exe" -ArgumentList $arguments -RedirectStandardError "$PWD\$label.log" -RedirectStandardOutput "$PWD\$label.out" -PassThru
  if (-not $p.WaitForExit(30000)) { Stop-Process -Id $p.Id -ErrorAction SilentlyContinue; Add-Content "$label.log" 'PROCESS_TIMEOUT_AFTER_30S' }
}
RunViewer '--replay crates\stream\tests\fixtures\decode-probe.av1 --codec av1 --width 256 --height 256 --fps 60 --seconds 3' 'av1-probe-final'
RunViewer '--replay crates\stream\tests\fixtures\decode-probe.hevc --codec hevc --width 256 --height 256 --fps 60 --seconds 3' 'hevc-probe-final'
RunViewer '--replay crates\stream\tests\fixtures\gray-bars-main10.hevc --codec hevc --width 64 --height 64 --fps 60 --seconds 3' 'main10-probe-final'
foreach ($case in @(@(1920,1080,60),@(1920,1080,120),@(1920,1080,240),@(3840,2160,60),@(3840,2160,120))) {
  $w,$h,$fps = $case
  $fixture = if ($w -eq 1920) { 'replay-1080.h264' } else { 'replay-4k.h264' }
  RunViewer "--replay $fixture --width $w --height $h --fps $fps --seconds 10" "replay-final-$w-$fps"
}
$env:LATCH_TEST_PATTERN = 'sweep'
$server = Start-Process -FilePath "$PWD\target\release\examples\serve.exe" -ArgumentList '--port 47882' -WindowStyle Hidden -RedirectStandardError "$PWD\serve-final.log" -RedirectStandardOutput "$PWD\serve-final.out" -PassThru
try {
  RunViewer '--port 47882 --width 1920 --height 1080 --fps 120 --seconds 15' 'loopback-final-1920-120'
} finally { Stop-Process -Id $server.Id -ErrorAction SilentlyContinue }
Set-Content probe-done.txt (Get-Date)
