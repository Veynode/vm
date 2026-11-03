param([int]$W,[int]$H,[string]$Out)
Add-Type @"
using System;using System.Runtime.InteropServices;
public class D{
 [StructLayout(LayoutKind.Sequential,CharSet=CharSet.Ansi)]public struct DM{
  [MarshalAs(UnmanagedType.ByValTStr,SizeConst=32)]public string dmDeviceName;
  public short dmSpecVersion,dmDriverVersion,dmSize,dmDriverExtra;public int dmFields;
  public int dmPositionX,dmPositionY,dmDisplayOrientation,dmDisplayFixedOutput;
  public short dmColor,dmDuplex,dmYResolution,dmTTOption,dmCollate;
  [MarshalAs(UnmanagedType.ByValTStr,SizeConst=32)]public string dmFormName;
  public short dmLogPixels;public int dmBitsPerPel,dmPelsWidth,dmPelsHeight,dmDisplayFlags,dmDisplayFrequency,dmICMMethod,dmICMIntent,dmMediaType,dmDitherType,dmReserved1,dmReserved2,dmPanningWidth,dmPanningHeight;}
 [DllImport("user32.dll")]public static extern bool EnumDisplaySettings(string n,int m,ref DM d);
 [DllImport("user32.dll")]public static extern int ChangeDisplaySettings(ref DM d,int f);
}
"@
$d=New-Object D+DM;$d.dmSize=[int][Runtime.InteropServices.Marshal]::SizeOf($d)
[void][D]::EnumDisplaySettings($null,-1,[ref]$d)
$d.dmPelsWidth=$W;$d.dmPelsHeight=$H;$d.dmFields=0x180000
$r=[D]::ChangeDisplaySettings([ref]$d,1)
[IO.File]::WriteAllText($Out,"result=$r now=$($d.dmPelsWidth)x$($d.dmPelsHeight)")
