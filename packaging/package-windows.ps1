# Builds the portable Windows package:
#   dist\Spate-<version>-windows-x64\      (folder you can run from)
#   dist\Spate-<version>-windows-x64.zip   (the same folder, zipped)
#
# Usage (PowerShell 7):  pwsh packaging\package-windows.ps1 [-SkipBuild]
param([switch]$SkipBuild)
$ErrorActionPreference = 'Stop'

$root = Split-Path $PSScriptRoot -Parent
$manifest = Join-Path $root 'Cargo.toml'
$version = ((cargo metadata --no-deps --format-version 1 --manifest-path $manifest | ConvertFrom-Json).packages |
    Where-Object name -EQ 'spate').version

if (-not $SkipBuild) {
    cargo build --release --manifest-path $manifest
    if ($LASTEXITCODE) { throw 'cargo build failed' }
    Push-Location (Join-Path $root 'extension')
    try {
        node build.mjs
        if ($LASTEXITCODE) { throw 'extension build failed' }
    } finally { Pop-Location }
}

$name = "Spate-$version-windows-x64"
$out = Join-Path $root "dist\$name"
if (Test-Path $out) { Remove-Item $out -Recurse -Force }
New-Item -ItemType Directory -Force (Join-Path $out 'extension') | Out-Null

Copy-Item (Join-Path $root 'target\release\spate.exe'), (Join-Path $root 'target\release\spate-cli.exe') $out
# The app's Options -> Browser integration page looks for these two folders.
Copy-Item (Join-Path $root 'extension\dist\chromium') (Join-Path $out 'extension\chromium') -Recurse
Copy-Item (Join-Path $root 'extension\dist\firefox') (Join-Path $out 'extension\firefox') -Recurse
Copy-Item (Join-Path $root 'packaging\QUICKSTART.txt') (Join-Path $out 'README.txt')
Copy-Item (Join-Path $root 'LICENSE') (Join-Path $out 'LICENSE.txt')

# Licence texts of every crate compiled into the executables (proc-macros and
# build-only dependencies run at compile time and are not shipped).
$registry = Join-Path ($env:CARGO_HOME ?? (Join-Path $HOME '.cargo')) 'registry\src'
$crates = cargo tree --manifest-path $manifest -p spate -p spate-cli -e normal,no-proc-macro --prefix none -f '{p}|{l}' |
    Where-Object { $_ } | ForEach-Object { $_ -replace ' \(\*\)$', '' } | Sort-Object -Unique
$authors = @{}
foreach ($p in (cargo metadata --format-version 1 --manifest-path $manifest | ConvertFrom-Json).packages) {
    $authors["$($p.name) $($p.version)"] = $p.authors -join ', '
}
# Standard texts for crates that ship no licence file, filled in from crates that do.
$appendix = [ordered]@{ 'Apache-2.0' = $null; 'BSL-1.0' = $null }
$used = @{}
$licenceName = '^(LICEN[CS]E|COPYING|COPYRIGHT|NOTICE|OFL|UFL)|licen[cs]e'

$notice = [System.Text.StringBuilder]::new()
[void]$notice.AppendLine("Spate $version bundles the following open-source libraries.")
[void]$notice.AppendLine("Their licence texts follow, as shipped in each library's source package.")
foreach ($line in $crates) {
    $pkg, $license = $line -split '\|', 2
    if ($pkg -match '\(') { continue } # workspace crates: covered by LICENSE.txt
    $crate, $ver = $pkg -split ' v', 2
    [void]$notice.AppendLine().AppendLine('=' * 78).AppendLine("$crate $ver  ($license)").AppendLine('=' * 78)
    $dir = Get-Item (Join-Path $registry "*\$crate-$ver") -ErrorAction SilentlyContinue | Select-Object -First 1
    $files = @()
    if ($dir) {
        $files = @(Get-ChildItem $dir.FullName -File | Where-Object Name -Match $licenceName | Sort-Object Name)
        if (-not $files) { # some crates keep them in a subfolder (e.g. bundled font licences)
            $files = @(Get-ChildItem $dir.FullName -File -Recurse | Where-Object { $_.Name -match $licenceName -and $_.Extension -ne '.rs' } | Sort-Object FullName)
        }
    }
    foreach ($f in $files) {
        $text = (Get-Content $f.FullName -Raw).TrimEnd()
        if (-not $appendix['Apache-2.0'] -and $f.Name -match '^LICEN[CS]E-APACHE' -and $text -match 'Apache License') { $appendix['Apache-2.0'] = $text }
        if (-not $appendix['BSL-1.0'] -and $text -match 'Boost Software License - Version 1\.0') { $appendix['BSL-1.0'] = $text }
        [void]$notice.AppendLine("--- $($f.FullName.Substring($dir.FullName.Length + 1)) ---").AppendLine($text)
    }
    $choice = if ($license -match 'Apache-2\.0') { 'Apache-2.0' } elseif ($license -match 'BSL-1\.0') { 'BSL-1.0' }
    if ($files -and -not ($license -match ' AND ' -and $choice)) { continue }
    $who = $authors["$crate $ver"]
    if ($who) { [void]$notice.AppendLine("Copyright (c) the $crate authors: $who") }
    if ($choice) {
        $used[$choice] = $true
        [void]$notice.AppendLine("Used under the $choice licence; its full text is at the end of this file.")
    } elseif (-not $files) {
        [void]$notice.AppendLine("This package ships no licence file; it is licensed under $license.")
    }
}
foreach ($id in $appendix.Keys) {
    if (-not $used[$id]) { continue }
    [void]$notice.AppendLine().AppendLine('=' * 78).AppendLine("Licence text: $id").AppendLine('=' * 78)
    [void]$notice.AppendLine($appendix[$id] ?? "See https://spdx.org/licenses/$id.html")
}
Set-Content (Join-Path $out 'THIRD-PARTY-LICENSES.txt') $notice.ToString() -Encoding utf8NoBOM

$zip = "$out.zip"
if (Test-Path $zip) { Remove-Item $zip -Force }
Compress-Archive -Path $out -DestinationPath $zip
$hash = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLowerInvariant()
# Same format as sha256sum, so `sha256sum -c` can check it.
Set-Content "$zip.sha256" "$hash  $(Split-Path $zip -Leaf)" -Encoding ascii -NoNewline
Write-Host "Package: $out"
Write-Host "Zip:     $zip ($([math]::Round((Get-Item $zip).Length / 1MB, 1)) MB)"
Write-Host "SHA-256: $hash"
