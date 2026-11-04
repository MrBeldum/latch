$ErrorActionPreference = 'Stop'
$env:CARGO_INCREMENTAL = '0'
$env:Path = "$env:USERPROFILE\.cargo\bin;" + $env:Path
Set-Location C:\Users\Daniel\dev\latch-gfn-c
$env:RUST_LOG = 'latch_stream=info,latch_client=info'
function RunViewer($arguments, $label) {
  $p = Start-Process -FilePath "$PWD\target\release\examples\present.exe" -ArgumentList $arguments -RedirectStandardError "$PWD\$label.log" -RedirectStandardOutput "$PWD\$label.out" -PassThru
  if (-not $p.WaitForExit(30000)) { Stop-Process -Id $p.Id -ErrorAction SilentlyContinue; Add-Content "$label.log" 'PROCESS_TIMEOUT_AFTER_30S' }
  Add-Content "$label.log" "exit_code=$($p.ExitCode) cpu_total_s=$($p.TotalProcessorTime.TotalSeconds)"
}
try {
  # Pixel readback uses the decoder after renderer installation (shared textures).
  # GPU pixel test was attempted separately; rustc crashed in dependencies.

  foreach ($run in @(@('h264-high','h264'),@('hevc','hevc'),@('av1','av1'))) {
    $codec,$ext = $run
    foreach ($case in @(@(1920,1080,60,'1080'),@(1920,1080,120,'1080'),@(1920,1080,240,'1080'),@(3840,2160,60,'4k'),@(3840,2160,120,'4k'))) {
      $w,$h,$fps,$name = $case
      foreach ($mode in @('cpu','gpu')) {
        $extra = if ($mode -eq 'cpu') { '--cpu' } else { '' }
        RunViewer "--replay stream-$name.$ext --codec $codec --width $w --height $h --fps $fps --seconds 6 $extra" "c3-$codec-$w-$fps-$mode"
      }
    }
  }
} catch { $_ | Out-File c3-error.txt } finally { Set-Content c3-done.txt (Get-Date) }
