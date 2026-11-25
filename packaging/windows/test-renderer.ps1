# Build the CI VM's software OpenGL driver. No system files or registry entries are changed.
param([ValidateSet('x86','x64','ARM64')][string]$Arch)
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true
if ($env:GITHUB_ACTIONS -ne 'true' -or $env:RUNNER_ENVIRONMENT -ne 'github-hosted') {
    throw 'This script requires a GitHub-hosted test VM.'
}
$work = New-Item -ItemType Directory (Join-Path $env:RUNNER_TEMP 'gp-test-renderer')
Set-Location $work
function Download-Verified($Uri, $File, $Sha256) {
    curl.exe --fail --silent --show-error --location --retry 3 $Uri --output $File
    if ((Get-FileHash $File).Hash -ne $Sha256) { throw "Digest mismatch for $File" }
}
Download-Verified 'https://archive.mesa3d.org/mesa-26.3.1.tar.xz' 'mesa.tar.xz' '4e4e438eac7b840a9383988cbb7c08dad8041da544b9319fc26d319fa7348961'
Download-Verified 'https://github.com/lexxmark/winflexbison/releases/download/v2.5.25/win_flex_bison-2.5.25.zip' 'flex.zip' '8d324b62be33604b2c45ad1dd34ab93d722534448f55a16ca7292de32b6ac135'
tar.exe -xf mesa.tar.xz
Expand-Archive flex.zip tools
python -m venv tools-venv
$python = "$work\tools-venv\Scripts\python.exe"
& $python -m pip install --disable-pip-version-check --only-binary=:all: 'meson==1.9.1' 'Mako==1.3.10' 'MarkupSafe==3.0.3' 'packaging==25.0' 'PyYAML==6.0.3'
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$vs = & $vswhere -latest -products '*' -property installationPath
Import-Module "$vs\Common7\Tools\Microsoft.VisualStudio.DevShell.dll"
$targetArch = $Arch.ToLowerInvariant()
Enter-VsDevShell -VsInstallPath $vs -SkipAutomaticLocation -DevCmdArguments "-host_arch=x64 -arch=$targetArch"
$env:PATH = "$work\tools;$work\tools-venv\Scripts;$env:PATH"
$env:CC = 'cl'
$env:CXX = 'cl'
meson setup build mesa-26.3.1 --buildtype=release --wrap-mode=nodownload -Db_vscrt=mt -Dgallium-drivers=softpipe '-Dvulkan-drivers=' '-Dplatforms=windows' -Dllvm=disabled -Dzstd=disabled -Dzlib=disabled -Dxmlconfig=disabled -Dexpat=disabled
ninja -C build src/gallium/targets/wgl/libgallium_wgl.dll src/gallium/targets/libgl-gdi/opengl32.dll
$dest = New-Item -ItemType Directory (Join-Path $work 'runtime')
foreach ($name in 'opengl32.dll','libgallium_wgl.dll') {
    $files = @(Get-ChildItem build -Recurse -File -Filter $name)
    if ($files.Count -ne 1) { throw "Expected one built $name" }
    Copy-Item $files[0].FullName $dest
}
Copy-Item -Recurse 'mesa-26.3.1/licenses' $dest
Copy-Item 'mesa-26.3.1/docs/license.rst' "$dest/Mesa-license.rst"
Copy-Item (Join-Path $PSScriptRoot 'software-renderer.md') "$dest/README.md"
$manifest = foreach ($name in 'opengl32.dll','libgallium_wgl.dll') {
    "$((Get-FileHash (Join-Path $dest $name)).Hash.ToLowerInvariant())  $name"
}
$manifest | Set-Content -Encoding ascii "$dest/SHA256SUMS.txt"
$artifact = New-Item -ItemType Directory (Join-Path $work 'artifact')
Compress-Archive -Path "$dest/*" -DestinationPath "$artifact/Mesa-26.3.1-Softpipe-Windows-$Arch.zip"
"GP_RENDERER_ARCHIVE=$artifact" | Out-File -Append -Encoding utf8 $env:GITHUB_ENV
"GP_TEST_RENDERER=$dest" | Out-File -Append -Encoding utf8 $env:GITHUB_ENV
