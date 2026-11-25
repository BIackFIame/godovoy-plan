$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true
if ($env:RENDERER_RUN -notmatch '^\d+$' -or $env:RENDERER_COMMIT -notmatch '^[a-f0-9]{40}$') {
    throw 'Specify a numeric run ID and the complete Git commit of the checked driver build.'
}
$repo = $env:GITHUB_REPOSITORY
$run = gh api "repos/$repo/actions/runs/$env:RENDERER_RUN" | ConvertFrom-Json
if ($run.head_repository.full_name -ne $repo -or $run.path -ne '.github/workflows/windows-packages.yml' -or
    $run.head_sha -ne $env:RENDERER_COMMIT -or $run.conclusion -ne 'success' -or $run.event -ne 'workflow_dispatch') {
    throw 'The software renderer does not come from the requested successful driver workflow.'
}
$arch = $env:PACKAGE_ARCH
$target = Join-Path $env:RUNNER_TEMP 'existing-mesa'
gh run download $env:RENDERER_RUN --repo $repo --name "software-opengl-$arch" --dir $target
$zip = Join-Path $target "Mesa-26.3.1-Softpipe-Windows-$arch.zip"
$output = Join-Path $target 'runtime'
Expand-Archive -LiteralPath $zip -DestinationPath $output
foreach ($name in 'opengl32.dll','libgallium_wgl.dll') {
    $hash = (Get-FileHash (Join-Path $output $name)).Hash.ToLowerInvariant()
    if ((Get-Content (Join-Path $output 'SHA256SUMS.txt')) -cnotcontains "$hash  $name") {
        throw "Incorrect internal digest for $name."
    }
}
"GP_TEST_RENDERER=$output" | Out-File -Append -Encoding utf8 $env:GITHUB_ENV
