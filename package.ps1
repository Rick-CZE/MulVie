# Assemble the portable MulVie folder: the exe + every non-system DLL it needs.
# (The app/exe/dist folder are named MulVie.)
# Paths are anchored to this script's own folder ($PSScriptRoot) so a repo
# rename/move can't break them.
$ErrorActionPreference = 'Stop'
$mingw = 'C:\Tools\mingw\mingw64\bin'
$root = $PSScriptRoot
$rel = Join-Path $root 'target\release\mulvie.exe'
$dist = Join-Path $root 'dist\MulVie'

if (-not (Test-Path $rel)) { Write-Output "release exe not built: $rel"; exit 1 }
New-Item -ItemType Directory -Path $dist -Force | Out-Null
Copy-Item $rel (Join-Path $dist 'mulvie.exe') -Force
Copy-Item (Join-Path $root 'libmpv\libmpv-2.dll') (Join-Path $dist 'libmpv-2.dll') -Force
Copy-Item (Join-Path $root 'pdfium\pdfium.dll') (Join-Path $dist 'pdfium.dll') -Force
# License files ship with every binary bundle: MulVie's MIT + the
# third-party notices/texts for the bundled libmpv and pdfium.
Copy-Item (Join-Path $root 'LICENSE') (Join-Path $dist 'LICENSE') -Force
Copy-Item (Join-Path $root 'packaging\licenses\THIRD-PARTY-LICENSES.txt') (Join-Path $dist 'THIRD-PARTY-LICENSES.txt') -Force

# Runtime files appear here whenever the dist exe is RUN (it doubles as a live
# install during testing) — they are the USER's data, so never delete them.
# But they must never ship: a release zip must contain ONLY the four app files
# (mulvie_libraries.json even holds private absolute paths). Zip from a list,
# not the folder:
#   Compress-Archive -Path $dist\mulvie.exe,$dist\libmpv-2.dll,$dist\pdfium.dll,$dist\README.txt ...
foreach ($j in 'mulvie_config.json', 'mulvie_libraries.json', 'mulvie_open.txt') {
    if (Test-Path (Join-Path $dist $j)) {
        Write-Output "WARNING: runtime file $j present in dist - EXCLUDE it from any release zip"
    }
}

# Inspect the exe's DLL imports and bundle the GNU-runtime ones it references.
$deps = & "$mingw\objdump.exe" -p (Join-Path $dist 'mulvie.exe') |
    Select-String 'DLL Name' | ForEach-Object { ($_ -split ':')[1].Trim() }
Write-Output "--- exe imports ---"
$deps | ForEach-Object { "  $_" }
foreach ($d in 'libgcc_s_seh-1.dll', 'libwinpthread-1.dll', 'libstdc++-6.dll') {
    if ($deps -contains $d) {
        $src = Join-Path $mingw $d
        if (Test-Path $src) { Copy-Item $src (Join-Path $dist $d) -Force; Write-Output "bundled $d" }
        else { Write-Output "!! MISSING $d in $mingw" }
    }
}

Write-Output "--- dist\MulVie ---"
Get-ChildItem $dist | ForEach-Object { "{0,-22} {1,8:N1} MB" -f $_.Name, ($_.Length / 1MB) }
"total: {0:N1} MB" -f ((Get-ChildItem $dist | Measure-Object Length -Sum).Sum / 1MB)

# One-file installer: a single self-unpacking exe embedding the four dist
# files gzipped (src/bin/installer.rs). Built LAST — its payload is the
# folder assembled above. (EAP relaxed around cargo: its progress goes to
# stderr, which PS 5.1 would otherwise convert into a terminating error.)
Write-Output "--- building one-file installer ---"
$env:PATH = "$env:USERPROFILE\.cargo\bin;$mingw;$env:PATH"
$eap = $ErrorActionPreference; $ErrorActionPreference = 'Continue'
cargo build --release --features installer --bin mulvie_installer 2>&1 |
    Select-Object -Last 1 | ForEach-Object { "$_" }
$ErrorActionPreference = $eap
if ($LASTEXITCODE -ne 0) { Write-Output 'installer build FAILED'; exit 1 }
$ver = (Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
$setup = Join-Path $root "dist\MulVie-$ver-setup.exe"
Copy-Item (Join-Path $root 'target\release\mulvie_installer.exe') $setup -Force
"installer: {0}  ({1:N1} MB)" -f $setup, ((Get-Item $setup).Length / 1MB)
