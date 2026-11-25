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
    $form = [Windows.Automation.PropertyCondition]::new(
        [Windows.Automation.AutomationElement]::NameProperty, 'Создать календарь')
    do {
        $window = [Windows.Automation.AutomationElement]::FromHandle($app.MainWindowHandle)
        $button = $window.FindFirst([Windows.Automation.TreeScope]::Descendants, $form)
        if ($null -ne $button) { break }
        if ($timer.Elapsed.TotalSeconds -gt 45) { throw 'Could not find the calendar creation action.' }
        Start-Sleep -Milliseconds 250
    } while (-not $app.HasExited)
    if ($null -eq $button) { throw 'The calendar creation form closed unexpectedly.' }
    Write-Output 'PASS: calendar creation action exposed to Windows UI Automation'
    if (-not $app.Responding) { throw 'Calendar window stopped responding.' }
    if (-not $app.CloseMainWindow() -or -not $app.WaitForExit(15000)) { throw 'Calendar did not close normally.' }
    if ($app.ExitCode -ne 0) { throw "Calendar exited with code $($app.ExitCode)." }
    Write-Output 'PASS: opened and closed native calendar window'
} finally {
    if (-not $app.HasExited) { $app.Kill(); $app.WaitForExit() }
    $app.Dispose()
}
