<#
.SYNOPSIS
    Measures the idle cost of a process over a time interval.

.DESCRIPTION
    Samples CPU time, working set, private bytes, handle count and thread count
    of a process before and after an interval, and the context switches of its
    threads through the "Thread" performance counters. Prints the deltas in a
    compact table. The script never stops, modifies or injects into the process.

    Performance counter names are localised on non-English Windows (for example
    French: "Changements de contexte/s"). The System.Diagnostics.PerformanceCounter
    API accepts the English names on every language of Windows, so the script
    uses them. The context switch row is omitted with a note if the counters
    cannot be read.

    Numbers are parsed and printed with the invariant culture.

.PARAMETER ProcessName
    Process name without the .exe extension. Default: naelbar.

.PARAMETER Seconds
    Length of the measurement interval in seconds. Default: 30.

.EXAMPLE
    .\measure-idle.ps1 -ProcessName naelbar -Seconds 30

.EXAMPLE
    .\measure-idle.ps1 -ProcessName explorer -Seconds 3

.OUTPUTS
    Exit code 0 on success. Exit code 1 if the process is not found, or cannot
    be sampled (for example because it exited during the interval).
#>
[CmdletBinding()]
param(
    [string]$ProcessName = 'naelbar',

    [ValidateRange(1, 86400)]
    [int]$Seconds = 30
)

$ErrorActionPreference = 'Stop'
$inv = [System.Globalization.CultureInfo]::InvariantCulture

function Format-Number {
    param([double]$Value, [string]$Format)
    return $Value.ToString($Format, $inv)
}

function Get-Snapshot {
    param([System.Diagnostics.Process]$Process)
    $Process.Refresh()
    return [pscustomobject]@{
        CpuMs      = $Process.TotalProcessorTime.TotalMilliseconds
        WorkingSet = [double]$Process.WorkingSet64
        Private    = [double]$Process.PrivateMemorySize64
        Handles    = [int]$Process.HandleCount
        Threads    = [int]$Process.Threads.Count
    }
}

# Opens one "Context Switches/sec" counter per thread that belongs to the process.
# Returns an object whose Counters property holds the open PerformanceCounter objects.
function Open-ContextSwitchCounters {
    param([int]$ProcessId, [string]$ImageName)

    # English counter names: PerformanceCounter accepts them on every Windows language.
    $category = 'Thread'
    $ctxName = 'Context Switches/sec'
    $idName = 'ID Process'

    $categoryObj = New-Object -TypeName System.Diagnostics.PerformanceCounterCategory -ArgumentList $category
    $allInstances = @($categoryObj.GetInstanceNames())

    # Thread instances are named "<process>/<n>" ("<process>#k/<n>" for duplicates).
    $pattern = '^' + [regex]::Escape($ImageName) + '(#\d+)?/'
    $candidates = @($allInstances | Where-Object { $_ -match $pattern })

    $owned = New-Object System.Collections.ArrayList
    foreach ($instance in $candidates) {
        $idCounter = New-Object -TypeName System.Diagnostics.PerformanceCounter -ArgumentList $category, $idName, $instance, $true
        try {
            $owner = [int]$idCounter.NextValue()
        } catch {
            $owner = -1
        } finally {
            $idCounter.Dispose()
        }
        if ($owner -eq $ProcessId) { [void]$owned.Add($instance) }
    }

    $counters = New-Object System.Collections.ArrayList
    foreach ($instance in $owned) {
        $counter = New-Object -TypeName System.Diagnostics.PerformanceCounter -ArgumentList $category, $ctxName, $instance, $true
        [void]$counters.Add([pscustomobject]@{ Counter = $counter; First = $counter.NextSample() })
    }

    return [pscustomobject]@{ Counters = $counters.ToArray() }
}

# Sum of the per-thread context switch rates, each computed over its own sampling
# window from the first sample (taken before the interval) to now.
function Read-ContextSwitchRate {
    param($Counters)
    $sum = 0.0
    $read = 0
    foreach ($entry in $Counters) {
        try {
            $second = $entry.Counter.NextSample()
            $sum += [double][System.Diagnostics.CounterSample]::Calculate($entry.First, $second)
            $read++
        } catch {
            # The thread exited during the interval: skip it.
        }
    }
    return [pscustomobject]@{ Rate = $sum; Read = $read }
}

# ---- Find the process -------------------------------------------------------

$name = $ProcessName -replace '\.exe$', ''
$found = @(Get-Process -Name $name -ErrorAction SilentlyContinue | Sort-Object -Property Id)
if ($found.Count -eq 0) {
    [Console]::Error.WriteLine("error: no process named '$name' is running.")
    exit 1
}
$process = $found[0]
if ($found.Count -gt 1) {
    Write-Warning ("{0} processes named '{1}': measuring PID {2}." -f $found.Count, $name, $process.Id)
}

# ---- Measure ----------------------------------------------------------------

$cpuCount = [Environment]::ProcessorCount
$context = $null
$contextNote = $null
try {
    $context = Open-ContextSwitchCounters -ProcessId $process.Id -ImageName $process.ProcessName
    if ($context.Counters.Count -eq 0) { $contextNote = 'no thread instances matched the process' }
} catch {
    $contextNote = $_.Exception.Message
}

$before = $null
$after = $null
$rate = $null
$timer = $null
$sampleError = $null
try {
    $before = Get-Snapshot -Process $process
    $timer = [System.Diagnostics.Stopwatch]::StartNew()
    Start-Sleep -Milliseconds ($Seconds * 1000)
    $after = Get-Snapshot -Process $process
    $timer.Stop()
    # Read the context switch counters after the timer stops: reading them takes time.
    if ($context -and $context.Counters.Count -gt 0) {
        $rate = Read-ContextSwitchRate -Counters $context.Counters
    }
} catch {
    $sampleError = $_.Exception.Message
} finally {
    if ($context) {
        foreach ($entry in $context.Counters) { $entry.Counter.Dispose() }
    }
}

if ($sampleError) {
    [Console]::Error.WriteLine("error: could not sample PID $($process.Id) ($name): $sampleError")
    exit 1
}

# ---- Report -----------------------------------------------------------------

$elapsedMs = $timer.Elapsed.TotalMilliseconds
$cpuDelta = $after.CpuMs - $before.CpuMs
$cpuPct = 100.0 * $cpuDelta / ($elapsedMs * $cpuCount)
$mb = 1048576.0

$ctxValue = 'n/a'
if ($rate -and $rate.Read -gt 0) {
    $ctxValue = Format-Number -Value $rate.Rate -Format '0.0'
}

$rows = @(
    [pscustomobject]@{
        Metric = 'CPU time (ms)'
        Start  = Format-Number -Value $before.CpuMs -Format '0.0'
        End    = Format-Number -Value $after.CpuMs -Format '0.0'
        Delta  = Format-Number -Value $cpuDelta -Format '+0.0;-0.0;0.0'
    }
    [pscustomobject]@{
        Metric = 'CPU % of total (avg)'
        Start  = '-'
        End    = '-'
        Delta  = Format-Number -Value $cpuPct -Format '0.000'
    }
    [pscustomobject]@{
        Metric = 'Working set (MB)'
        Start  = Format-Number -Value ($before.WorkingSet / $mb) -Format '0.0'
        End    = Format-Number -Value ($after.WorkingSet / $mb) -Format '0.0'
        Delta  = Format-Number -Value (($after.WorkingSet - $before.WorkingSet) / $mb) -Format '+0.0;-0.0;0.0'
    }
    [pscustomobject]@{
        Metric = 'Private bytes (MB)'
        Start  = Format-Number -Value ($before.Private / $mb) -Format '0.0'
        End    = Format-Number -Value ($after.Private / $mb) -Format '0.0'
        Delta  = Format-Number -Value (($after.Private - $before.Private) / $mb) -Format '+0.0;-0.0;0.0'
    }
    [pscustomobject]@{
        Metric = 'Handles'
        Start  = Format-Number -Value $before.Handles -Format '0'
        End    = Format-Number -Value $after.Handles -Format '0'
        Delta  = Format-Number -Value ($after.Handles - $before.Handles) -Format '+0;-0;0'
    }
    [pscustomobject]@{
        Metric = 'Threads'
        Start  = Format-Number -Value $before.Threads -Format '0'
        End    = Format-Number -Value $after.Threads -Format '0'
        Delta  = Format-Number -Value ($after.Threads - $before.Threads) -Format '+0;-0;0'
    }
    [pscustomobject]@{
        Metric = 'Context switches/s (avg)'
        Start  = '-'
        End    = '-'
        Delta  = $ctxValue
    }
)

Write-Output ("{0} (PID {1}), interval {2} s, {3} logical CPUs" -f $process.ProcessName, $process.Id, (Format-Number -Value ($elapsedMs / 1000.0) -Format '0.0'), $cpuCount)
$rows | Format-Table -AutoSize -Property Metric, Start, End, Delta
if ($ctxValue -eq 'n/a') {
    $note = 'no thread counter data'
    if ($contextNote) { $note = $contextNote }
    Write-Output "Note: context switches omitted ($note)."
}
exit 0
