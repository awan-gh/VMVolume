# Build VMVolume.exe. With -Publish: bump the version, build, tag and upload a GitHub release
# (installed copies auto-update from the latest release's VMVolume.exe asset).
param([string]$Version, [switch]$Publish)
$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot
if ($Version) { (Get-Content Cargo.toml) -replace '^version = ".*"', "version = `"$Version`"" | Set-Content Cargo.toml -Encoding utf8 }
cargo test --release; if ($LASTEXITCODE) { throw 'tests failed' }
cargo build --release; if ($LASTEXITCODE) { throw 'build failed' }
$v = (Select-String -Path Cargo.toml -Pattern '^version = "(.*)"').Matches[0].Groups[1].Value
New-Item -ItemType Directory -Force dist | Out-Null
Copy-Item target\release\VMVolume.exe dist\VMVolume.exe -Force
"Built VMVolume $v -> dist\VMVolume.exe"
if ($Publish) {
    if (-not $Version) { throw 'pass -Version x.y.z with -Publish' }
    git add Cargo.toml Cargo.lock; git commit -m "Release $v"; git tag "v$v"; git push; git push --tags
    gh release create "v$v" dist\VMVolume.exe --title "VMVolume $v" --generate-notes
}
