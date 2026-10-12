param(
    [Parameter(Mandatory = $true)][string]$InputDirectory,
    [Parameter(Mandatory = $true)][string]$ReportDirectory
)
$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force -Path $ReportDirectory | Out-Null
$files = @(Get-ChildItem $InputDirectory -Recurse -Filter '*.exe')
if ($files.Count -eq 0) { throw 'No executable files found to inspect.' }
$inventory = @($files | ForEach-Object {
    $signature = Get-AuthenticodeSignature -LiteralPath $_.FullName
    [ordered]@{
        file = $_.FullName.Substring((Resolve-Path $InputDirectory).Path.Length).TrimStart('\')
        size = $_.Length
        sha256 = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash
        signature = $signature.Status.ToString()
        signer = if ($signature.SignerCertificate) { $signature.SignerCertificate.Subject } else { $null }
    }
})
$inventory | ConvertTo-Json -Depth 5 | Set-Content (Join-Path $ReportDirectory 'files.json')

try {
    # GitHub's disposable Windows image may ship with Defender disabled.
    # Enable it for this scan; never change protection on a user's computer.
    $policy = 'HKLM:\SOFTWARE\Policies\Microsoft\Windows Defender'
    if (Test-Path $policy) {
        foreach ($name in @('DisableAntiSpyware', 'DisableAntiVirus')) {
            if (Get-ItemProperty -Path $policy -Name $name -ErrorAction SilentlyContinue) {
                Set-ItemProperty -Path $policy -Name $name -Value 0
            }
        }
    }
    $service = Get-Service WinDefend
    if ($service.Status -ne 'Running') {
        Set-Service WinDefend -StartupType Automatic
        Start-Service WinDefend
    }
    Update-MpSignature
    $status = Get-MpComputerStatus
    $status | Select-Object AMServiceEnabled, AntivirusEnabled, AMEngineVersion, AntivirusSignatureVersion, AntivirusSignatureLastUpdated |
        ConvertTo-Json | Set-Content (Join-Path $ReportDirectory 'defender-version.json')
    if (-not $status.AMServiceEnabled -or -not $status.AntivirusEnabled) {
        throw 'Defender is unavailable. No clean verdict can be issued.'
    }
    $tool = Get-ChildItem "$env:ProgramData\Microsoft\Windows Defender\Platform\*\MpCmdRun.exe" |
        Sort-Object FullName -Descending | Select-Object -First 1
    $scanner = if ($tool) { $tool.FullName } else { "$env:ProgramFiles\Windows Defender\MpCmdRun.exe" }
    # Report detections without quarantining the evidence. This option applies
    # only to this custom scan; real-time protection is not turned off.
    & $scanner -Scan -ScanType 3 -File (Resolve-Path $InputDirectory).Path -DisableRemediation 2>&1 |
        Tee-Object -FilePath (Join-Path $ReportDirectory 'defender-scan.txt')
    $scanExit = $LASTEXITCODE
    [ordered]@{scanner='Microsoft Defender';exit_code=$scanExit;clean=($scanExit -eq 0)} |
        ConvertTo-Json | Set-Content (Join-Path $ReportDirectory 'verdict.json')
    if ($scanExit -ne 0) { throw "Defender detected a threat or could not finish scanning (exit $scanExit). See the scan report." }
} catch {
    $_ | Out-String | Set-Content (Join-Path $ReportDirectory 'error.txt')
    throw
}
