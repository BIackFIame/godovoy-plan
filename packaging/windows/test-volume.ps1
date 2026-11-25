# Only used in a disposable GitHub VM. Give the test vault its own NTFS drive.
$ErrorActionPreference = 'Stop'
if ($env:GITHUB_ACTIONS -ne 'true' -or $env:RUNNER_ENVIRONMENT -ne 'github-hosted') {
    throw 'This script requires a GitHub-hosted test VM.'
}
$letter = 'R','S','T','U','V','W' | Where-Object { -not (Test-Path "${_}:\") } | Select-Object -First 1
if (-not $letter) { throw 'No unused test drive letter.' }
$marker = [Guid]::NewGuid().ToString('N')
$disk = Join-Path $env:RUNNER_TEMP "$marker.vhd"
$instructions = Join-Path $env:RUNNER_TEMP "$marker.txt"
@"
create vdisk file="$disk" maximum=256 type=expandable
select vdisk file="$disk"
attach vdisk
create partition primary
format fs=ntfs quick label=$marker
assign letter=$letter
"@ | Set-Content -Encoding ascii $instructions
diskpart.exe /s $instructions
if ($LASTEXITCODE -ne 0) { throw 'Could not create the temporary test disk.' }
$root = "${letter}:\"
$volume = [IO.DriveInfo]::new($root)
if ($volume.VolumeLabel -ne $marker -or $volume.DriveFormat -ne 'NTFS') { throw 'Unexpected test volume.' }
$acl = [Security.AccessControl.DirectorySecurity]::new()
$user = [Security.Principal.WindowsIdentity]::GetCurrent().User
$acl.SetOwner($user)
$acl.SetAccessRuleProtection($true, $false)
foreach ($sid in $user.Value, 'S-1-5-18', 'S-1-5-32-544') {
    $identity = [Security.Principal.SecurityIdentifier]::new($sid)
    $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($identity, 'FullControl', 'ContainerInherit,ObjectInherit', 'None', 'Allow'))
}
Set-Acl -LiteralPath $root -AclObject $acl
"GP_TEST_VOLUME=$root" | Out-File -Append -Encoding utf8 $env:GITHUB_ENV
