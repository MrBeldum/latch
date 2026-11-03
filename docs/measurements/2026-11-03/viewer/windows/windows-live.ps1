Set-Location C:\Users\Daniel\dev\latch-gfn-c
$env:RUST_LOG='latch_stream=info,latch_client=info'
$p=Start-Process "$PWD\target\release\examples\present.exe" -ArgumentList '100.70.121.94 --port 47850 --width 1920 --height 1080 --fps 120 --seconds 10' -RedirectStandardError "$PWD\c3-linux-live.log" -RedirectStandardOutput "$PWD\c3-linux-live.out" -PassThru
if (-not $p.WaitForExit(30000)) { Stop-Process -Id $p.Id -ErrorAction SilentlyContinue; Add-Content c3-linux-live.log 'PROCESS_TIMEOUT_AFTER_30S' }
Set-Content c3-live-done.txt (Get-Date)
