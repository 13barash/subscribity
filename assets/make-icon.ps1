# Generates assets/icon.ico (16-256 px, PNG-compressed entries) for Subscribity.
# Run: powershell -NoProfile -ExecutionPolicy Bypass -File assets\make-icon.ps1
Add-Type -AssemblyName System.Drawing
$ErrorActionPreference = 'Stop'
$out = Join-Path $PSScriptRoot 'icon.ico'
$sizes = 16, 20, 24, 32, 40, 48, 64, 128, 256

function New-RoundRect([float]$x, [float]$y, [float]$w, [float]$h, [float]$r) {
    $p = New-Object System.Drawing.Drawing2D.GraphicsPath
    $d = $r * 2
    $p.AddArc($x, $y, $d, $d, 180, 90)
    $p.AddArc($x + $w - $d, $y, $d, $d, 270, 90)
    $p.AddArc($x + $w - $d, $y + $h - $d, $d, $d, 0, 90)
    $p.AddArc($x, $y + $h - $d, $d, $d, 90, 90)
    $p.CloseFigure()
    $p
}

function New-IconPng([int]$s) {
    $bmp = New-Object System.Drawing.Bitmap $s, $s, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = 'AntiAlias'
    $g.PixelOffsetMode = 'HighQuality'
    $g.Clear([System.Drawing.Color]::Transparent)
    $k = $s / 256.0

    # Background: rounded square, violet -> indigo gradient.
    $m = [Math]::Max(0.5, 8 * $k)
    $bg = New-RoundRect $m $m ($s - 2 * $m) ($s - 2 * $m) (56 * $k)
    $grad = New-Object System.Drawing.Drawing2D.LinearGradientBrush (New-Object System.Drawing.PointF 0, 0), (New-Object System.Drawing.PointF $s, $s), ([System.Drawing.Color]::FromArgb(255, 139, 92, 246)), ([System.Drawing.Color]::FromArgb(255, 67, 56, 202))
    $g.FillPath($grad, $bg)

    # Renewal ring: a circular arrow around the centre.
    $white = [System.Drawing.Color]::White
    $cx = $s / 2.0; $cy = $s / 2.0; $r = 78 * $k
    $pen = New-Object System.Drawing.Pen $white, ([Math]::Max(1.5, 22 * $k))
    $pen.StartCap = 'Round'
    $g.DrawArc($pen, $cx - $r, $cy - $r, 2 * $r, 2 * $r, -60, 300)
    # Arrow head at the arc start (angle -60 deg), pointing into the gap (counter-clockwise).
    $pen.StartCap = 'Flat'
    $a = -60 * [Math]::PI / 180
    $ex = $cx + $r * [Math]::Cos($a); $ey = $cy + $r * [Math]::Sin($a)
    $h = 30 * $k
    $tip = New-Object System.Drawing.PointF ($ex + 40 * $k * [Math]::Cos($a - [Math]::PI / 2)), ($ey + 40 * $k * [Math]::Sin($a - [Math]::PI / 2))
    $p1 = New-Object System.Drawing.PointF ($ex + $h * [Math]::Cos($a)), ($ey + $h * [Math]::Sin($a))
    $p2 = New-Object System.Drawing.PointF ($ex - $h * [Math]::Cos($a)), ($ey - $h * [Math]::Sin($a))
    $g.FillPolygon((New-Object System.Drawing.SolidBrush $white), [System.Drawing.PointF[]]@($tip, $p1, $p2))

    # Centre coin with a dollar-ish "S" bar (omitted below 24 px, where it turns to mush).
    if ($s -ge 24) {
        $cr = 40 * $k
        $g.FillEllipse((New-Object System.Drawing.SolidBrush $white), $cx - $cr, $cy - $cr, 2 * $cr, 2 * $cr)
        $font = New-Object System.Drawing.Font 'Segoe UI', ([float](52 * $k)), ([System.Drawing.FontStyle]::Bold), ([System.Drawing.GraphicsUnit]::Pixel)
        $fmt = New-Object System.Drawing.StringFormat
        $fmt.Alignment = 'Center'; $fmt.LineAlignment = 'Center'
        $g.TextRenderingHint = 'AntiAliasGridFit'
        $g.DrawString('$', $font, (New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(255, 79, 70, 229))), (New-Object System.Drawing.RectangleF ($cx - $cr), ($cy - $cr + 2 * $k), (2 * $cr), (2 * $cr)), $fmt)
    } else {
        $cr = 32 * $k
        $g.FillEllipse((New-Object System.Drawing.SolidBrush $white), $cx - $cr, $cy - $cr, 2 * $cr, 2 * $cr)
    }
    $g.Dispose()
    $ms = New-Object System.IO.MemoryStream
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    , $ms.ToArray()
}

$pngs = foreach ($s in $sizes) { , (New-IconPng $s) }
$fs = [System.IO.File]::Create($out)
$w = New-Object System.IO.BinaryWriter $fs
$w.Write([uint16]0); $w.Write([uint16]1); $w.Write([uint16]$sizes.Count)
$offset = 6 + 16 * $sizes.Count
for ($i = 0; $i -lt $sizes.Count; $i++) {
    $s = $sizes[$i]; $b = if ($s -ge 256) { 0 } else { $s }
    $w.Write([byte]$b); $w.Write([byte]$b); $w.Write([byte]0); $w.Write([byte]0)
    $w.Write([uint16]1); $w.Write([uint16]32)
    $w.Write([uint32]$pngs[$i].Length); $w.Write([uint32]$offset)
    $offset += $pngs[$i].Length
}
foreach ($p in $pngs) { $w.Write($p) }
$w.Close()
# Preview for review (not shipped).
[System.IO.File]::WriteAllBytes((Join-Path $PSScriptRoot '..\..\.test\icon_256.png'), $pngs[-1])
[System.IO.File]::WriteAllBytes((Join-Path $PSScriptRoot '..\..\.test\icon_32.png'), $pngs[3])
"wrote $out"
