# Observe only the GUI process launched for this disposable test profile.
param([Parameter(Mandatory)][string]$Executable, [Parameter(Mandatory)][string]$ProfilePath)
$ErrorActionPreference = 'Stop'
$start = [Diagnostics.ProcessStartInfo]::new($Executable)
$start.UseShellExecute = $false
foreach ($arg in '--data-dir', $ProfilePath, '--listen', '127.0.0.1:0', '--no-discovery') {
    $start.ArgumentList.Add($arg)
}
$app = [Diagnostics.Process]::Start($start)
try {
    $timer = [Diagnostics.Stopwatch]::StartNew()
    do {
        Start-Sleep -Milliseconds 250
        $app.Refresh()
        if ($app.HasExited) { throw "GUI exited before its window opened (code $($app.ExitCode))." }
        if ($timer.Elapsed.TotalSeconds -gt 45) { throw 'Timed out waiting for the calendar window.' }
    } until ($app.MainWindowHandle -ne [IntPtr]::Zero -and $app.MainWindowTitle -like '*Годовой план*')
    Add-Type -AssemblyName UIAutomationClient
    Add-Type -AssemblyName UIAutomationTypes
    function Find-Control([string]$Name, [Windows.Automation.ControlType]$Type) {
        $condition = [Windows.Automation.AndCondition]::new(
            [Windows.Automation.PropertyCondition]::new([Windows.Automation.AutomationElement]::NameProperty, $Name),
            [Windows.Automation.PropertyCondition]::new([Windows.Automation.AutomationElement]::ControlTypeProperty, $Type))
        do {
            $app.Refresh()
            if ($app.HasExited) { throw "GUI closed while waiting for $Name." }
            $window = [Windows.Automation.AutomationElement]::FromHandle($app.MainWindowHandle)
            $control = $window.FindFirst([Windows.Automation.TreeScope]::Descendants, $condition)
            if ($null -ne $control) { return $control }
            Start-Sleep -Milliseconds 200
        } while ($timer.Elapsed.TotalSeconds -lt 50)
        throw "Could not find $Name in the calendar window."
    }
    function Activate-Choice($Control) {
        $pattern = $null
        if ($Control.TryGetCurrentPattern([Windows.Automation.TogglePattern]::Pattern, [ref]$pattern)) {
            $pattern.Toggle()
        } elseif ($Control.TryGetCurrentPattern([Windows.Automation.InvokePattern]::Pattern, [ref]$pattern)) {
            $pattern.Invoke()
        } else {
            throw "The calendar choice does not expose a supported action."
        }
    }
    $button = [Windows.Automation.ControlType]::Button
    $edit = [Windows.Automation.ControlType]::Edit
    $null = Find-Control 'Ваше имя' $edit
    $null = Find-Control 'Название календаря' $edit
    $null = Find-Control 'Создать календарь' $button
    $join = Find-Control 'Присоединиться' $button
    Activate-Choice $join
    $null = Find-Control 'Код приглашения' $edit
    $create = Find-Control 'Новый календарь' $button
    Activate-Choice $create
    $null = Find-Control 'Название календаря' $edit
    Write-Output 'PASS: opened the join form and returned to the creation form'
    $app.Refresh()
    $directory = [IO.Path]::GetDirectoryName($Executable)
    foreach ($library in 'opengl32.dll', 'libgallium_wgl.dll') {
        $expected = [IO.Path]::GetFullPath((Join-Path $directory $library))
        if (-not ($app.Modules | Where-Object { $_.FileName -ieq $expected })) {
            throw "The test process did not load the packaged $library."
        }
    }
    Write-Output 'PASS: the calendar process loaded both expected software-renderer DLLs'
    if (-not $app.Responding) { throw 'Calendar window stopped responding.' }
    if (-not $app.CloseMainWindow() -or -not $app.WaitForExit(15000)) { throw 'Calendar did not close normally.' }
    if ($app.ExitCode -ne 0) { throw "Calendar exited with code $($app.ExitCode)." }
    Write-Output 'PASS: opened and closed native calendar window'
} finally {
    if (-not $app.HasExited) { $app.Kill(); $app.WaitForExit() }
    $app.Dispose()
}
