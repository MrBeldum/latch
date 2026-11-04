$ErrorActionPreference='Stop'
$ProgressPreference='SilentlyContinue'
Add-Type -ReferencedAssemblies System.Windows.Forms,System.Drawing @'
using System;
using System.Drawing;
using System.Windows.Forms;
using System.Diagnostics;
using System.Runtime.InteropServices;
public class VddMotion : Form {
 protected override bool ShowWithoutActivation { get { return true; } }
 protected override CreateParams CreateParams { get { var p=base.CreateParams;p.ExStyle|=0x08000000;return p; } }
 [DllImport("dwmapi.dll")] static extern int DwmFlush();
 public static void Run() {
  using(var f=new VddMotion()) {
   f.FormBorderStyle=FormBorderStyle.None;f.ShowInTaskbar=false;f.StartPosition=FormStartPosition.Manual;f.Bounds=new Rectangle(0,0,640,360);
   f.TopMost=true;f.BackColor=Color.Green;f.Show(); var sw=Stopwatch.StartNew(); int i=0;
   while(sw.Elapsed.TotalSeconds<75) {
    using(var g=f.CreateGraphics()) {g.Clear(Color.FromArgb(24,24,24));using(var b=new SolidBrush(Color.FromArgb(64,160,224)))g.FillRectangle(b,(i++*8)%600,80,40,180);}
    Application.DoEvents();DwmFlush();
   }
   f.Close();
  }
 }
}
'@
$root='C:\Users\Daniel\dev\latch-fix-vdd'
$server=Start-Process "$root\target\release\examples\serve.exe" -ArgumentList '--bind 100.106.23.40 --port 47897 --prepare-display' -WindowStyle Hidden -RedirectStandardError "$root\serve.log" -RedirectStandardOutput "$root\serve.out" -PassThru
$server.Id | Set-Content "$root\serve.pid"
try { [VddMotion]::Run() } finally {Stop-Process -Id $server.Id -ErrorAction SilentlyContinue}
