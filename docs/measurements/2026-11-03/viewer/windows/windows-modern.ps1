$ErrorActionPreference = 'Stop'
Set-Location C:\Users\Daniel\dev\latch-gfn-c
$env:RUST_LOG = 'latch_stream=info'
foreach ($codec in @('av1','hevc')) {
  foreach ($case in @(@(1920,1080,60),@(1920,1080,120),@(1920,1080,240),@(3840,2160,60),@(3840,2160,120))) {
    $w,$h,$fps = $case
    $fixture = if ($w -eq 1920) { "replay-1080.$codec" } else { "replay-4k.$codec" }
    $label = "modern-$codec-$w-$fps"
    $p=Start-Process -FilePath "$PWD\target\release\examples\present.exe" -ArgumentList "--replay $fixture --codec $codec --width $w --height $h --fps $fps --seconds 5" -RedirectStandardError "$PWD\$label.log" -RedirectStandardOutput "$PWD\$label.out" -PassThru
    if (-not $p.WaitForExit(30000)) { Stop-Process -Id $p.Id; Add-Content "$label.log" 'PROCESS_TIMEOUT_AFTER_30S' }
  }
}
Set-Content modern-done.txt (Get-Date)
