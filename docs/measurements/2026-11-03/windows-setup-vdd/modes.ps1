Add-Type @'
using System;
using System.Runtime.InteropServices;
public class VddModes {
 [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern bool EnumDisplayDevicesW(string device,uint index,[In,Out] byte[] data,uint flags);
 [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern bool EnumDisplaySettingsW(string device,int index,[In,Out] byte[] data);
 public static void Dump() {
  for(uint i=0;;i++) {
   byte[] d=new byte[840]; BitConverter.GetBytes(840).CopyTo(d,0);
   if(!EnumDisplayDevicesW(null,i,d,0)) break;
   string name=System.Text.Encoding.Unicode.GetString(d,4,64).TrimEnd('\0');
   Console.WriteLine(name+" "+System.Text.Encoding.Unicode.GetString(d,68,256).TrimEnd('\0'));
   byte[] m=new byte[220]; BitConverter.GetBytes((short)220).CopyTo(m,68);
   if(EnumDisplaySettingsW(name,-1,m)) Console.WriteLine("CURRENT "+BitConverter.ToInt32(m,172)+"x"+BitConverter.ToInt32(m,176)+"@"+BitConverter.ToInt32(m,184));
   for(int j=0;;j++) {
    m=new byte[220]; BitConverter.GetBytes((short)220).CopyTo(m,68);
    if(!EnumDisplaySettingsW(name,j,m)) break;
    Console.WriteLine(BitConverter.ToInt32(m,172)+"x"+BitConverter.ToInt32(m,176)+"@"+BitConverter.ToInt32(m,184));
   }
  }
 }
}
'@
[VddModes]::Dump()
