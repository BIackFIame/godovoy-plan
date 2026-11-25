# Invoked with Windows PowerShell, no user profile, no modules and a fixed
# script. The profile path is data, never parsed as PowerShell source.
$ErrorActionPreference = 'Stop'
try {
    $profilePath = [IO.Path]::GetFullPath($env:GODOVOY_PROFILE_ACL)
    $driveRoot = [IO.Path]::GetPathRoot($profilePath)
    if ($driveRoot -notmatch '^[A-Za-z]:\\$') { exit 22 }
    $drive = [IO.DriveInfo]::new($driveRoot)
    if ($drive.DriveType -ne 'Fixed' -or $drive.DriveFormat -ne 'NTFS') { exit 22 }
    $user = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    $trusted = @($user, 'S-1-5-18', 'S-1-5-32-544')
    $installer = 'S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464'
    function Assert-Private([string] $path, [bool] $isDirectory, [bool] $ancestor) {
        $attrs = [IO.File]::GetAttributes($path)
        if (($attrs -band [IO.FileAttributes]::ReparsePoint) -ne 0) { exit 22 }
        $sections = [Security.AccessControl.AccessControlSections]'Owner,Access'
        if ($isDirectory) {
            $security = [IO.Directory]::GetAccessControl($path, $sections)
        } else {
            $security = [IO.File]::GetAccessControl($path, $sections)
        }
        $descriptor = [Security.AccessControl.RawSecurityDescriptor]::new($security.GetSecurityDescriptorBinaryForm(), 0)
        if ($null -eq $descriptor.Owner -or $null -eq $descriptor.DiscretionaryAcl) { exit 21 }
        if ($trusted -notcontains $descriptor.Owner.Value -and -not ($ancestor -and $descriptor.Owner.Value -eq $installer)) { exit 21 }
        $fileInheritance = $false
        $directoryInheritance = $false
        foreach ($ace in $descriptor.DiscretionaryAcl) {
            if ($ace -isnot [Security.AccessControl.QualifiedAce]) { exit 21 }
            if ($ace.AceQualifier -eq 'AccessDenied') { continue }
            if ($ace.AceQualifier -ne 'AccessAllowed') { exit 21 }
            $inheritOnly = ([int]$ace.AceFlags -band [int][Security.AccessControl.AceFlags]::InheritOnly) -ne 0
            $sid = $ace.SecurityIdentifier.Value
            if ($trusted -contains $sid -or ($inheritOnly -and $sid -eq 'S-1-3-0')) {
                # Children need a real inherited DACL; otherwise the token's
                # default DACL can apply. Propagate through future backup dirs.
                if (([int]$ace.AceFlags -band [int][Security.AccessControl.AceFlags]::NoPropagateInherit) -eq 0) {
                    if (([int]$ace.AceFlags -band [int][Security.AccessControl.AceFlags]::ObjectInherit) -ne 0) { $fileInheritance = $true }
                    if (([int]$ace.AceFlags -band [int][Security.AccessControl.AceFlags]::ContainerInherit) -ne 0) { $directoryInheritance = $true }
                }
                continue
            }
            if (-not $ancestor) { exit 21 }
            if ($inheritOnly -or $sid -eq $installer) { continue }
            # DELETE_CHILD, DELETE, WRITE_DAC, WRITE_OWNER, WRITE_ATTRIBUTES,
            # GENERIC_WRITE and GENERIC_ALL on an ancestor permit path changes.
            if (($ace.AccessMask -band 0x500D0140) -ne 0) { exit 23 }
        }
        if ($isDirectory -and -not $ancestor -and (-not $fileInheritance -or -not $directoryInheritance)) { exit 21 }
    }
    $parent = [IO.Directory]::GetParent($profilePath)
    if ($env:GODOVOY_CREATE_PROFILE -eq '1' -and -not [IO.Directory]::Exists($profilePath)) {
        # Create parent directories without storing any application data there.
        [void][IO.Directory]::CreateDirectory($parent.FullName)
    }
    while ($null -ne $parent) {
        Assert-Private $parent.FullName $true $true
        $parent = $parent.Parent
    }
    function Check-Folder([string] $folder) {
        Assert-Private $folder $true $false
        foreach ($entry in [IO.Directory]::EnumerateFileSystemEntries($folder)) {
            $attr = [IO.File]::GetAttributes($entry)
            if (($attr -band [IO.FileAttributes]::ReparsePoint) -ne 0) { exit 22 }
            if (($attr -band [IO.FileAttributes]::Directory) -eq 0) {
                Assert-Private $entry $false $false
            }
        }
    }
    if ($env:GODOVOY_CREATE_PROFILE -eq '1' -and -not [IO.Directory]::Exists($profilePath)) {
        $acl = [Security.AccessControl.DirectorySecurity]::new()
        $acl.SetAccessRuleProtection($true, $false)
        foreach ($sid in $trusted) {
            $identity = [Security.Principal.SecurityIdentifier]::new($sid)
            $rule = [Security.AccessControl.FileSystemAccessRule]::new($identity, 'FullControl', 'ContainerInherit,ObjectInherit', 'None', 'Allow')
            $acl.AddAccessRule($rule)
        }
        [void][IO.Directory]::CreateDirectory($profilePath, $acl)
    }
    Check-Folder $profilePath
    $backups = [IO.Path]::Combine($profilePath, 'backups')
    if ([IO.Directory]::Exists($backups)) { Check-Folder $backups }
    exit 0
} catch {
    exit 24
}
