<#
.SYNOPSIS
  Generates assets\icon.ico and assets\icon-256.png from the geometry of assets\icon.svg.

.DESCRIPTION
  Draws the Explorer Session Recall icon with System.Drawing (GDI+): an amber
  folder with two tabs on its top edge (darker back panel, lighter front
  panel) and a blue circular "recall" badge at the bottom right holding a
  white counter-clockwise arrow. The path data below is copied from
  icon.svg, which is the design source of truth; keep the two in sync.

  Every frame is rendered directly at its target size (16, 20, 24, 32, 40,
  48, 64, 128, 256 px) by scaling the 256-unit geometry, not downscaled from
  the 256 px image. Frames of 24 px and below use a simplified variant:
  solid fills (each gradient's mid colour), a badge enlarged to about 60% of
  the icon width (anchored at the SVG badge's bottom-right extent), and a
  thicker arrow so the badge stays recognisable.

  The ICO container is written by this script: ICONDIR, one ICONDIRENTRY per
  frame, then the payloads. The 256 px frame is stored as PNG; the others as
  32-bit BGRA DIBs (BITMAPINFOHEADER, doubled height, bottom-up rows, 1-bpp
  AND mask), the layout Windows' own icons use.

  The outputs are committed, so building esr.exe and Setup needs no image
  tools. Re-run after changing the design:

    powershell -NoProfile -ExecutionPolicy Bypass -File assets\make-icon.ps1
#>
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2
Add-Type -AssemblyName System.Drawing

$Sizes = 16, 20, 24, 32, 40, 48, 64, 128, 256
$IcoPath = Join-Path $PSScriptRoot 'icon.ico'
$PngPath = Join-Path $PSScriptRoot 'icon-256.png'

# --- Geometry and paint, copied from icon.svg (256 x 256 user units) --------
$BackPath  = 'M24 64 a12 12 0 0 1 12 -12 h48 a12 12 0 0 1 10 5 l8 11 h4 l6 -11 a12 12 0 0 1 10 -5 h44 a12 12 0 0 1 10 5 l8 11 h36 a12 12 0 0 1 12 12 v116 a14 14 0 0 1 -14 14 H38 a14 14 0 0 1 -14 -14 Z'
$FrontPath = 'M24 98 a14 14 0 0 1 14 -14 h180 a14 14 0 0 1 14 14 v94 a14 14 0 0 1 -14 14 H38 a14 14 0 0 1 -14 -14 Z'
$ArrowPath = 'M159 197 A30 30 0 1 0 168 162'
$ArrowHead = 'M152 146 L172 150 L164 169 Z'
$BackStops  = '#E8A317', '#C98400'   # vertical, over the back path's bounding box
$FrontStops = '#FFD866', '#F5B82E'   # vertical, over the front path's bounding box
$BadgeStops = '#3B8EEA', '#1A5FC8'   # diagonal top-left -> bottom-right, over the r51 circle's box
$BadgeCx = 186; $BadgeCy = 186; $RingR = 58; $BadgeR = 51
$ArrowWidth = 12; $HeadWidth = 6
# Small variant: badge outer diameter as a fraction of the icon width, and the
# arrow stroke in target pixels (the plain scaled stroke would be ~1 px at
# 16 px; about 2 px fills the badge with white and hides the blue disk).
# Anchor = bottom-right extent of the white ring.
$SmallMax = 24
$SmallBadgeFraction = 0.6
$SmallArrowPx = @{ 16 = 1.6; 20 = 1.7; 24 = 1.8 }
$BadgeAnchor = $BadgeCx + $RingR

function ConvertTo-Color([string]$hex) {
    [System.Drawing.ColorTranslator]::FromHtml($hex)
}

function Get-MidColor([string[]]$stops) {
    $a = ConvertTo-Color $stops[0]; $b = ConvertTo-Color $stops[1]
    [System.Drawing.Color]::FromArgb(255,
        [int][math]::Round(($a.R + $b.R) / 2), [int][math]::Round(($a.G + $b.G) / 2), [int][math]::Round(($a.B + $b.B) / 2))
}

# SVG circular arc (rx = ry, no rotation) from (x1,y1) to (x2,y2) -> GraphicsPath.AddArc.
# Endpoint-to-center conversion per SVG 1.1 implementation notes F.6.5; GDI+
# angles are degrees clockwise from +x in y-down space, like SVG's.
function Add-SvgArc($path, [double]$x1, [double]$y1, [double]$r, [int]$large, [int]$sweep, [double]$x2, [double]$y2) {
    $hx = ($x1 - $x2) / 2; $hy = ($y1 - $y2) / 2
    $d2 = $hx * $hx + $hy * $hy
    if ($d2 -eq 0) { return }
    if ($d2 -gt $r * $r) { $r = [math]::Sqrt($d2) }
    $coef = [math]::Sqrt([math]::Max(0, $r * $r - $d2) / $d2)
    if ($large -eq $sweep) { $coef = -$coef }
    $cx = $coef * $hy + ($x1 + $x2) / 2
    $cy = -$coef * $hx + ($y1 + $y2) / 2
    $a1 = [math]::Atan2($y1 - $cy, $x1 - $cx) * 180 / [math]::PI
    $a2 = [math]::Atan2($y2 - $cy, $x2 - $cx) * 180 / [math]::PI
    $delta = $a2 - $a1
    if ($sweep -eq 1 -and $delta -lt 0) { $delta += 360 }
    if ($sweep -eq 0 -and $delta -gt 0) { $delta -= 360 }
    $path.AddArc([single]($cx - $r), [single]($cy - $r), [single](2 * $r), [single](2 * $r), [single]$a1, [single]$delta)
}

# Builds a GraphicsPath from the SVG path subset icon.svg uses: M/L/H/V/A/Z
# and their relative forms (M -> StartFigure, L/H/V -> AddLine,
# A -> Add-SvgArc, Z -> CloseFigure).
function New-SvgPath([string]$d) {
    $path = New-Object System.Drawing.Drawing2D.GraphicsPath
    $tokens = [regex]::Matches($d, '[A-Za-z]|-?\d+(?:\.\d+)?') | ForEach-Object { $_.Value }
    $i = 0; $x = 0.0; $y = 0.0; $sx = 0.0; $sy = 0.0; $cmd = ''
    while ($i -lt $tokens.Count) {
        if ($tokens[$i] -match '^[A-Za-z]$') { $cmd = $tokens[$i]; $i++ }
        $rel = $cmd -cmatch '^[a-z]$'
        switch -CaseSensitive ($cmd.ToUpperInvariant()) {
            'M' {
                $nx = [double]$tokens[$i]; $ny = [double]$tokens[$i + 1]; $i += 2
                if ($rel) { $nx += $x; $ny += $y }
                $path.StartFigure(); $x = $nx; $y = $ny; $sx = $x; $sy = $y
            }
            'L' {
                $nx = [double]$tokens[$i]; $ny = [double]$tokens[$i + 1]; $i += 2
                if ($rel) { $nx += $x; $ny += $y }
                $path.AddLine([single]$x, [single]$y, [single]$nx, [single]$ny); $x = $nx; $y = $ny
            }
            'H' {
                $nx = [double]$tokens[$i]; $i++
                if ($rel) { $nx += $x }
                $path.AddLine([single]$x, [single]$y, [single]$nx, [single]$y); $x = $nx
            }
            'V' {
                $ny = [double]$tokens[$i]; $i++
                if ($rel) { $ny += $y }
                $path.AddLine([single]$x, [single]$y, [single]$x, [single]$ny); $y = $ny
            }
            'A' {
                $rx = [double]$tokens[$i]; $ry = [double]$tokens[$i + 1]; $rot = [double]$tokens[$i + 2]
                $large = [int]$tokens[$i + 3]; $sweep = [int]$tokens[$i + 4]
                $nx = [double]$tokens[$i + 5]; $ny = [double]$tokens[$i + 6]; $i += 7
                if ($rx -ne $ry -or $rot -ne 0) { throw "only circular, unrotated arcs are supported: $d" }
                if ($rel) { $nx += $x; $ny += $y }
                Add-SvgArc $path $x $y $rx $large $sweep $nx $ny; $x = $nx; $y = $ny
            }
            'Z' { $path.CloseFigure(); $x = $sx; $y = $sy }
            default { throw "unsupported SVG path command '$cmd' in: $d" }
        }
    }
    $path
}

# Vertical (or diagonal) two-stop gradient from p1 to p2, like an SVG
# linearGradient in objectBoundingBox units. TileFlipXY so antialiased edge
# pixels just outside the box get the end colour instead of wrapping.
function New-Fill([string[]]$stops, [bool]$solid, [double]$x1, [double]$y1, [double]$x2, [double]$y2) {
    if ($solid) { return New-Object System.Drawing.SolidBrush (Get-MidColor $stops) }
    $b = New-Object System.Drawing.Drawing2D.LinearGradientBrush(
        (New-Object System.Drawing.PointF([single]$x1, [single]$y1)),
        (New-Object System.Drawing.PointF([single]$x2, [single]$y2)),
        (ConvertTo-Color $stops[0]), (ConvertTo-Color $stops[1]))
    $b.WrapMode = [System.Drawing.Drawing2D.WrapMode]::TileFlipXY
    $b
}

function Draw-Icon([System.Drawing.Graphics]$g, [int]$size) {
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    $g.CompositingQuality = [System.Drawing.Drawing2D.CompositingQuality]::HighQuality
    $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $g.Clear([System.Drawing.Color]::Transparent)
    $g.ScaleTransform([single]($size / 256), [single]($size / 256))
    $small = $size -le $SmallMax
    $disposables = New-Object System.Collections.ArrayList

    try {
        # 1. <path fill="url(#back)">: folder back panel with both tabs; gradient over its box (y 52..210).
        $p = New-SvgPath $BackPath; [void]$disposables.Add($p)
        $b = New-Fill $BackStops $small 0 52 0 210; [void]$disposables.Add($b)
        $g.FillPath($b, $p)

        # 2. <path fill="url(#front)">: front panel; gradient over its box (y 84..206).
        $p = New-SvgPath $FrontPath; [void]$disposables.Add($p)
        $b = New-Fill $FrontStops $small 0 84 0 206; [void]$disposables.Add($b)
        $g.FillPath($b, $p)

        # Badge group. Small variant: scale it about its bottom-right anchor so
        # its outer diameter is ~60% of the icon width (transforms prepend, so
        # points are moved to the anchor, scaled, and moved back).
        $k = 1.0
        if ($small) {
            $k = ($SmallBadgeFraction * 256) / (2 * $RingR)
            $g.TranslateTransform([single]$BadgeAnchor, [single]$BadgeAnchor)
            $g.ScaleTransform([single]$k, [single]$k)
            $g.TranslateTransform([single](-$BadgeAnchor), [single](-$BadgeAnchor))
        }

        # 3. <circle r="58" fill="#FFFFFF">: white ring separating the badge from the folder.
        $b = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::White); [void]$disposables.Add($b)
        $g.FillEllipse($b, [single]($BadgeCx - $RingR), [single]($BadgeCy - $RingR), [single](2 * $RingR), [single](2 * $RingR))

        # 4. <circle r="51" fill="url(#badge)">: blue disk; gradient (0,0)->(1,1) of its box.
        $b = New-Fill $BadgeStops $small ($BadgeCx - $BadgeR) ($BadgeCy - $BadgeR) ($BadgeCx + $BadgeR) ($BadgeCy + $BadgeR)
        [void]$disposables.Add($b)
        $g.FillEllipse($b, [single]($BadgeCx - $BadgeR), [single]($BadgeCy - $BadgeR), [single](2 * $BadgeR), [single](2 * $BadgeR))

        # 5. <path stroke-width="12" stroke-linecap="round">: counter-clockwise arc (large arc, sweep 0).
        $width = $ArrowWidth
        if ($small) { $width = [math]::Max($ArrowWidth, ($SmallArrowPx[$size] * 256 / $size) / $k) }
        $pen = New-Object System.Drawing.Pen ([System.Drawing.Color]::White), ([single]$width); [void]$disposables.Add($pen)
        $pen.StartCap = [System.Drawing.Drawing2D.LineCap]::Round
        $pen.EndCap = [System.Drawing.Drawing2D.LineCap]::Round
        $p = New-SvgPath $ArrowPath; [void]$disposables.Add($p)
        $g.DrawPath($pen, $p)

        # 6. <path fill stroke-width="6" stroke-linejoin="round">: arrowhead, filled then stroked.
        $p = New-SvgPath $ArrowHead; [void]$disposables.Add($p)
        $white = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::White); [void]$disposables.Add($white)
        $g.FillPath($white, $p)
        $headPen = New-Object System.Drawing.Pen ([System.Drawing.Color]::White), ([single]$HeadWidth); [void]$disposables.Add($headPen)
        $headPen.LineJoin = [System.Drawing.Drawing2D.LineJoin]::Round
        $g.DrawPath($headPen, $p)
    }
    finally {
        foreach ($d in $disposables) { $d.Dispose() }
    }
}

function New-IconBitmap([int]$size) {
    $bmp = New-Object System.Drawing.Bitmap($size, $size, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    try { Draw-Icon $g $size } finally { $g.Dispose() }
    $bmp
}

# 32-bit DIB icon payload: BITMAPINFOHEADER (height doubled for XOR + AND),
# straight-alpha BGRA rows bottom-up, then the 1-bpp AND mask (1 = transparent), bottom-up.
function Get-DibPayload([System.Drawing.Bitmap]$bmp) {
    $s = $bmp.Width
    $rect = New-Object System.Drawing.Rectangle(0, 0, $s, $s)
    $data = $bmp.LockBits($rect, [System.Drawing.Imaging.ImageLockMode]::ReadOnly, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    try {
        $stride = $data.Stride
        $pixels = New-Object byte[] ($stride * $s)
        [System.Runtime.InteropServices.Marshal]::Copy($data.Scan0, $pixels, 0, $pixels.Length)
    }
    finally { $bmp.UnlockBits($data) }

    $maskRow = [int]([math]::Floor(($s + 31) / 32) * 4)
    $xorSize = $s * $s * 4
    $ms = New-Object System.IO.MemoryStream
    $w = New-Object System.IO.BinaryWriter($ms)
    $w.Write([uint32]40); $w.Write([int32]$s); $w.Write([int32](2 * $s))
    $w.Write([uint16]1); $w.Write([uint16]32); $w.Write([uint32]0)
    $w.Write([uint32]($xorSize + $maskRow * $s))
    $w.Write([int32]0); $w.Write([int32]0); $w.Write([uint32]0); $w.Write([uint32]0)
    for ($y = $s - 1; $y -ge 0; $y--) { $w.Write($pixels, $y * $stride, $s * 4) }
    for ($y = $s - 1; $y -ge 0; $y--) {
        $row = New-Object byte[] $maskRow
        for ($x = 0; $x -lt $s; $x++) {
            if ($pixels[$y * $stride + $x * 4 + 3] -eq 0) { $row[[int][math]::Floor($x / 8)] = $row[[int][math]::Floor($x / 8)] -bor (0x80 -shr ($x % 8)) }
        }
        $w.Write($row)
    }
    $w.Flush()
    $bytes = $ms.ToArray()
    $w.Dispose()
    , $bytes
}

function Get-PngPayload([System.Drawing.Bitmap]$bmp) {
    $ms = New-Object System.IO.MemoryStream
    try { $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png); , $ms.ToArray() } finally { $ms.Dispose() }
}

$bitmaps = @{}
try {
    $payloads = New-Object System.Collections.ArrayList
    foreach ($s in $Sizes) {
        $bmp = New-IconBitmap $s
        $bitmaps[$s] = $bmp
        if ($s -eq 256) { [void]$payloads.Add((Get-PngPayload $bmp)) } else { [void]$payloads.Add((Get-DibPayload $bmp)) }
    }

    $fs = [System.IO.File]::Create($IcoPath)
    $w = New-Object System.IO.BinaryWriter($fs)
    try {
        # ICONDIR
        $w.Write([uint16]0); $w.Write([uint16]1); $w.Write([uint16]$Sizes.Count)
        # ICONDIRENTRY x N
        $offset = 6 + 16 * $Sizes.Count
        for ($n = 0; $n -lt $Sizes.Count; $n++) {
            $s = $Sizes[$n]; $len = $payloads[$n].Length
            $dim = [byte]0
            if ($s -lt 256) { $dim = [byte]$s }
            $w.Write($dim); $w.Write($dim); $w.Write([byte]0); $w.Write([byte]0)
            $w.Write([uint16]1); $w.Write([uint16]32); $w.Write([uint32]$len); $w.Write([uint32]$offset)
            $offset += $len
        }
        foreach ($payload in $payloads) { $w.Write([byte[]]$payload) }
    }
    finally { $w.Dispose() }

    $bitmaps[256].Save($PngPath, [System.Drawing.Imaging.ImageFormat]::Png)
    Write-Host "wrote $IcoPath ($((Get-Item $IcoPath).Length) bytes, sizes $($Sizes -join ', '))"
    Write-Host "wrote $PngPath"
}
finally {
    foreach ($bmp in $bitmaps.Values) { $bmp.Dispose() }
}
