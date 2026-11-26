$ErrorActionPreference = 'Stop'
if ($env:GITHUB_ACTIONS -ne 'true' -or $env:RUNNER_ARCH -ne 'X64' -or !$env:RUNNER_TEMP) {
    throw 'The disposable WSL fixture runs in the Windows x64 CI job'
}

$fixture = Join-Path $env:RUNNER_TEMP "visor-wsl-$env:GITHUB_RUN_ID-$env:GITHUB_RUN_ATTEMPT"
$distro = "VisorCI-$env:GITHUB_RUN_ID-$env:GITHUB_RUN_ATTEMPT"
$archive = Join-Path $env:RUNNER_TEMP 'visor-wsl-artifact/visor-wsl.tar'
$workspace = '/tmp/visor-wsl/project'
$codeProcess = $null
$owner = $null
$imported = $false

function Invoke-WSL {
    param([Parameter(ValueFromRemainingArguments = $true)][string[]]$Arguments)
    & wsl.exe --distribution $distro --exec @Arguments
    if ($LASTEXITCODE -ne 0) { throw "WSL fixture command failed ($LASTEXITCODE)" }
}

New-Item -ItemType Directory -Path $fixture | Out-Null
try {
    $install = Join-Path $fixture 'wsl'
    New-Item -ItemType Directory -Path $install | Out-Null
    & wsl.exe --import $distro $install $archive --version 2
    if ($LASTEXITCODE -ne 0) { throw "WSL 2 could not import the disposable CI distribution ($LASTEXITCODE)" }
    $imported = $true
    & wsl.exe --list --verbose
    $revisions = Invoke-WSL cat /opt/visor-wsl/SOURCE_REVISIONS
    $revisions | Write-Host
    if ($revisions -notcontains "visor=$env:GITHUB_SHA") { throw 'WSL fixture source revision differs from this CI job' }
    $owner = Start-Process -FilePath 'wsl.exe' -ArgumentList @(
        '--distribution', $distro, '--user', 'root', '--exec', '/usr/bin/python3', '/opt/visor-wsl/task.py'
    ) -PassThru -RedirectStandardOutput (Join-Path $fixture 'owner.log') -RedirectStandardError (Join-Path $fixture 'owner-error.log')

    $case = $null
    for ($attempt = 0; $attempt -lt 60; $attempt++) {
        if ($owner.HasExited) { throw "Installed WSL runtime exited ($($owner.ExitCode))" }
        $raw = & wsl.exe --distribution $distro --exec sh -c 'test ! -s /tmp/visor-wsl/case.json || cat /tmp/visor-wsl/case.json'
        if ($LASTEXITCODE -eq 0 -and $raw) {
            $case = $raw | ConvertFrom-Json
            break
        }
        Start-Sleep -Milliseconds 500
    }
    if (!$case -or $case.workspace -ne $workspace) { throw 'The scoped CLI task did not complete' }

    $codeVersion = (Get-Content -LiteralPath 'ci/wsl/vscode-version' -Raw).Trim()
    $zip = Join-Path $fixture 'vscode.zip'
    $url = "https://update.code.visualstudio.com/$codeVersion/win32-x64-archive/stable"
    & curl.exe --fail --silent --show-error --location --retry 3 --max-time 180 $url --output $zip
    if ($LASTEXITCODE -ne 0) { throw 'VS Code desktop download failed' }
    $codeDir = Join-Path $fixture 'code'
    Expand-Archive -LiteralPath $zip -DestinationPath $codeDir
    $executable = Join-Path $codeDir 'Code.exe'
    $signature = Get-AuthenticodeSignature -FilePath $executable
    if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -notmatch 'Microsoft Corporation') {
        throw "VS Code desktop signature failed: $($signature.Status)"
    }
    $userData = Join-Path $fixture 'user-data'
    $extensions = Join-Path $fixture 'extensions'
    New-Item -ItemType Directory -Path (Join-Path $userData 'User'), $extensions | Out-Null
    @{
        'workbench.startupEditor' = 'none'
        'editor.accessibilitySupport' = 'on'
        'security.workspace.trust.startupPrompt' = 'always'
        'extensions.autoUpdate' = $false
        'update.mode' = 'none'
        'telemetry.telemetryLevel' = 'off'
        'window.dialogStyle' = 'custom'
    } | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $userData 'User/settings.json') -Encoding utf8
    $codeCli = Join-Path $codeDir 'bin/code.cmd'
    & $codeCli --user-data-dir $userData --extensions-dir $extensions --install-extension ms-vscode-remote.remote-wsl --force
    if ($LASTEXITCODE -ne 0) { throw 'VS Code Remote WSL installation failed' }
    & $codeCli --user-data-dir $userData --extensions-dir $extensions --list-extensions --show-versions

    $listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
    $listener.Start()
    $port = $listener.LocalEndpoint.Port
    $listener.Stop()
    $codeProcess = Start-Process -FilePath $executable -PassThru -ArgumentList @(
        ('--user-data-dir="{0}"' -f $userData), ('--extensions-dir="{0}"' -f $extensions),
        '--remote-debugging-address=127.0.0.1', "--remote-debugging-port=$port",
        '--skip-welcome', '--skip-release-notes', '--locale=en-US', '--disable-gpu',
        "--folder-uri=vscode-remote://wsl+$distro$workspace"
    )
    $result = & node ci/wsl/editor.mjs $port $distro $workspace $case.cliTask $case.marker
    $result | Write-Host
    if ($LASTEXITCODE -ne 0) { throw 'The VS Code WSL workspace check failed' }
    $task = $result | Where-Object { $_ -match '^VISOR_EDITOR_TASK=([a-f0-9-]{36})$' }
    if (@($task).Count -ne 1) { throw 'VS Code task ID missing' }
    Invoke-WSL /usr/bin/python3 /opt/visor-wsl/task.py verify ($task -replace '^VISOR_EDITOR_TASK=', '')
} finally {
    if ($codeProcess -and !$codeProcess.HasExited) {
        $codeProcess.Kill($true)
        $codeProcess.WaitForExit()
    }
    if ($imported) {
        & wsl.exe --unregister $distro
        if ($LASTEXITCODE -ne 0) { throw "Could not remove disposable distribution $distro" }
    }
    if ($owner) { $owner.WaitForExit(10000) | Out-Null }
    foreach ($log in @('owner.log', 'owner-error.log')) {
        $path = Join-Path $fixture $log
        if (Test-Path -LiteralPath $path) { Get-Content -LiteralPath $path -Tail 40 }
    }
    Remove-Item -LiteralPath $fixture -Recurse -Force
}
