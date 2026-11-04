Set-Location C:\Users\Daniel\dev\latch-gfn-c
$ff = 'C:\ffmpeg\bin\ffmpeg.exe'
foreach ($r in @(@('1080','1920x1080','30M'),@('4k','3840x2160','80M'))) {
  $n,$size,$rate = $r
  & $ff -v error -y -f lavfi -i "testsrc2=size=${size}:rate=60" -frames:v 240 -pix_fmt yuv420p -c:v libaom-av1 -usage realtime -cpu-used 8 -lag-in-frames 0 -row-mt 1 -tiles 4x4 -g 1000 -b:v $rate -f obu "stream-$n.av1"
}
Set-Content gen-done.txt (Get-Date)
