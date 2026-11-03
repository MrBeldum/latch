Set-Location C:\Users\Daniel\dev\latch-gfn-c
$env:RUST_LOG = 'latch_stream=info'
$p=Start-Process -FilePath "$PWD\target\release\examples\present.exe" -ArgumentList '--replay probe-main10.hevc --codec hevc --width 256 --height 256 --fps 120 --seconds 5' -RedirectStandardError "$PWD\main10-256-final.log" -RedirectStandardOutput "$PWD\main10-256-final.out" -PassThru
if (-not $p.WaitForExit(30000)) { Stop-Process -Id $p.Id; Add-Content main10-256-final.log 'PROCESS_TIMEOUT_AFTER_30S' }
