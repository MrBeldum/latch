$ErrorActionPreference = 'Stop'
Set-Location C:\Users\Daniel\dev\latch-gfn-c
Add-Type -AssemblyName System.Drawing
Add-Type @'
using System;
using System.Runtime.InteropServices;
public class LatchViewerRect {
 [StructLayout(LayoutKind.Sequential)] public struct Rect { public int Left, Top, Right, Bottom; }
 [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr hwnd, IntPtr insertAfter, int x, int y, int cx, int cy, uint flags);
 [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hwnd, out Rect rect);
}
'@
try {
 foreach ($run in @(@('h264-high','h264'),@('hevc','hevc'),@('av1','av1'))) {
  $codec,$ext=$run
  $p=Start-Process "$PWD\target\release\examples\present.exe" -ArgumentList "--replay crates\stream\tests\fixtures\decode-probe.$ext --codec $codec --width 256 --height 256 --fps 60 --seconds 5" -RedirectStandardError "$PWD\c3-pixels-$codec.log" -RedirectStandardOutput "$PWD\c3-pixels-$codec.out" -PassThru
  try {
   Start-Sleep -Seconds 2
   $p.Refresh()
   # Raise only this temporary test window, without changing keyboard focus.
   [LatchViewerRect]::SetWindowPos($p.MainWindowHandle,[IntPtr](-1),0,0,0,0,0x11) | Out-Null
   Start-Sleep -Milliseconds 250
   $rect=New-Object LatchViewerRect+Rect
   if (-not [LatchViewerRect]::GetWindowRect($p.MainWindowHandle,[ref]$rect)) { throw "no viewer window $codec" }
   $bmp=New-Object System.Drawing.Bitmap(($rect.Right-$rect.Left),($rect.Bottom-$rect.Top))
   $g=[System.Drawing.Graphics]::FromImage($bmp)
   try {
    $g.CopyFromScreen($rect.Left,$rect.Top,0,0,$bmp.Size)
    $bmp.Save("$PWD\c3-pixels-$codec.png",[System.Drawing.Imaging.ImageFormat]::Png)
   } finally { $g.Dispose(); $bmp.Dispose() }
   if (-not $p.WaitForExit(10000)) { Stop-Process -Id $p.Id }
  } finally { if (-not $p.HasExited) { Stop-Process -Id $p.Id -ErrorAction SilentlyContinue } }
 }
} catch { $_ | Out-File c3-pixels-error.txt } finally { Set-Content c3-pixels-done.txt (Get-Date) }
