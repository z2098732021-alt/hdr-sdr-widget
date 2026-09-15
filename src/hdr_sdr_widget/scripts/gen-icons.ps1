# 生成应用图标：32x32.png / 128x128.png / icon.ico / tray.ico
# 视觉：单色扁平太阳（Liquid Glass 深色主题下的暖金色），透明背景。
# icon.ico 使用 PNG 压缩 ICO（Windows Vista+ 支持），内含 128 与 32 两个尺寸。

param(
    [string]$IconsDir = (Join-Path $PSScriptRoot "..\src-tauri\icons")
)

Add-Type -AssemblyName System.Drawing

$Gold = [System.Drawing.Color]::FromArgb(255, 255, 205, 77)
$Dim  = [System.Drawing.Color]::FromArgb(255, 214, 175, 92)

function New-IconBitmap {
    param([int]$Size, [System.Drawing.Color]$Color)
    $bmp = New-Object System.Drawing.Bitmap($Size, $Size, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $g.Clear([System.Drawing.Color]::Transparent)

    $cx = $Size / 2.0
    $cy = $Size / 2.0
    $coreR = $Size * 0.26
    $rayStart = $Size * 0.34
    $rayEnd = $Size * 0.42
    $penWidth = [Math]::Max(1.4, $Size * 0.07)

    $brush = New-Object System.Drawing.SolidBrush($Color)
    for ($i = 0; $i -lt 8; $i++) {
        $angle = $i * [Math]::PI / 4
        $x1 = $cx + [Math]::Cos($angle) * $rayStart
        $y1 = $cy + [Math]::Sin($angle) * $rayStart
        $x2 = $cx + [Math]::Cos($angle) * $rayEnd
        $y2 = $cy + [Math]::Sin($angle) * $rayEnd
        $pen = New-Object System.Drawing.Pen($Color, [float]$penWidth)
        $pen.StartCap = [System.Drawing.Drawing2D.LineCap]::Round
        $pen.EndCap = [System.Drawing.Drawing2D.LineCap]::Round
        $g.DrawLine($pen, [float]$x1, [float]$y1, [float]$x2, [float]$y2)
        $pen.Dispose()
    }
    $g.FillEllipse($brush, [float]($cx - $coreR), [float]($cy - $coreR), [float]($coreR * 2), [float]($coreR * 2))

    $g.Dispose()
    $brush.Dispose()
    return $bmp
}

function New-IconFromPngs {
    # PNG 压缩 ICO：把多个 PNG 封装进单 ICO 文件。
    param([string]$IcoPath, [byte[]]$Png32, [byte[]]$Png128)
    $count = 2
    $offset = 6 + 16 * $count
    $ms = New-Object System.IO.MemoryStream
    $bw = New-Object System.IO.BinaryWriter($ms)
    $bw.Write([UInt16]0)                       # reserved
    $bw.Write([UInt16]1)                       # type: icon
    $bw.Write([UInt16]$count)                  # image count
    # entry 1: 128x128
    $bw.Write([Byte]0); $bw.Write([Byte]0)     # 0 表示 256，但 128 也常编码为 0/128；用 128 显式
    $bw.Write([Byte]128); $bw.Write([Byte]0); $bw.Write([Byte]0)
    $bw.Write([UInt16]1); $bw.Write([UInt16]32)
    $bw.Write([UInt32]$Png128.Length); $bw.Write([UInt32]$offset)
    $offset += $Png128.Length
    # entry 2: 32x32
    $bw.Write([Byte]32); $bw.Write([Byte]32); $bw.Write([Byte]0); $bw.Write([Byte]0)
    $bw.Write([UInt16]1); $bw.Write([UInt16]32)
    $bw.Write([UInt32]$Png32.Length); $bw.Write([UInt32]$offset)
    # 数据
    $bw.Write($Png128); $bw.Write($Png32)
    $bw.Flush()
    [System.IO.File]::WriteAllBytes($IcoPath, $ms.ToArray())
    $bw.Dispose(); $ms.Dispose()
}

function Save-Png {
    param([System.Drawing.Bitmap]$Bmp, [string]$Path)
    $Bmp.Save($Path, [System.Drawing.Imaging.ImageFormat]::Png)
}

# 确保目录存在
New-Item -ItemType Directory -Force -Path $IconsDir | Out-Null

$png32 = New-IconBitmap -Size 32 -Color $Gold
$png128 = New-IconBitmap -Size 128 -Color $Gold
$pngTray = New-IconBitmap -Size 32 -Color $Dim

$p32 = Join-Path $IconsDir "32x32.png"
$p128 = Join-Path $IconsDir "128x128.png"
$pIco = Join-Path $IconsDir "icon.ico"
$pTrayIco = Join-Path $IconsDir "tray.ico"

Save-Png -Bmp $png32 -Path $p32
Save-Png -Bmp $png128 -Path $p128
Save-Png -Bmp $pngTray -Path $pTrayIco

# 转 PNG 字节用于 ICO 封装
$bytes32 = [System.IO.File]::ReadAllBytes($p32)
$bytes128 = [System.IO.File]::ReadAllBytes($p128)
New-IconFromPngs -IcoPath $pIco -Png32 $bytes32 -Png128 $bytes128

$png32.Dispose(); $png128.Dispose(); $pngTray.Dispose()

Write-Output "图标已生成到: $IconsDir"
Get-ChildItem $IconsDir | ForEach-Object { Write-Output ("  {0}  {1} bytes" -f $_.Name, $_.Length) }
