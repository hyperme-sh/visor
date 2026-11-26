param([switch]$RemoteSsh)

$ErrorActionPreference = 'Stop'
if ($env:GITHUB_ACTIONS -ne 'true' -or $env:RUNNER_ARCH -ne 'X64' -or !$env:RUNNER_TEMP) {
    throw 'The disposable WSL fixture runs in the Windows x64 CI job'
}

$fixture = Join-Path $env:RUNNER_TEMP "visor-wsl-$env:GITHUB_RUN_ID-$env:GITHUB_RUN_ATTEMPT"
$distro = "VisorCI-$env:GITHUB_RUN_ID-$env:GITHUB_RUN_ATTEMPT"
if ($RemoteSsh) {
    $fixture += '-ssh'
    $distro += '-SSH'
}
$archive = Join-Path $env:RUNNER_TEMP 'visor-wsl-artifact/visor-wsl.tar'
$workspace = '/home/visorci/.visor-wsl/project'
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
    $ownerArgs = @('--distribution', $distro, '--user', 'root', '--exec', '/usr/bin/python3')
    if ($RemoteSsh) {
        $identity = Join-Path $fixture 'client_key'
        & ssh-keygen.exe -q -t ed25519 -N '' -f $identity
        if ($LASTEXITCODE -ne 0) { throw 'Fixture SSH client key generation failed' }
        $guestKey = Invoke-WSL wslpath -a "$identity.pub"
        $ownerArgs += @('/opt/visor-wsl/ssh_host.py', ('"{0}"' -f $guestKey))
    } else {
        $ownerArgs += '/opt/visor-wsl/task.py'
    }
    $owner = Start-Process -FilePath 'wsl.exe' -ArgumentList $ownerArgs -PassThru -RedirectStandardOutput (Join-Path $fixture 'owner.log') -RedirectStandardError (Join-Path $fixture 'owner-error.log')

    $case = $null
    for ($attempt = 0; $attempt -lt 60; $attempt++) {
        if ($owner.HasExited) { throw "Installed WSL runtime exited ($($owner.ExitCode))" }
        $raw = & wsl.exe --distribution $distro --exec sh -c 'test ! -s /home/visorci/.visor-wsl/case.json || cat /home/visorci/.visor-wsl/case.json'
        if ($LASTEXITCODE -eq 0 -and $raw) {
            $case = $raw | ConvertFrom-Json
            break
        }
        Start-Sleep -Milliseconds 500
    }
    if (!$case -or $case.workspace -ne $workspace) { throw 'The scoped CLI task did not complete' }

    $remoteName = $distro
    $transport = 'wsl'
    if ($RemoteSsh) {
        if ($case.device -notmatch '^[a-f0-9-]{36}$') { throw 'Installed fixture device ID missing' }
        $remoteName = 'visor-ci'
        $transport = 'ssh-remote'
        $hostKey = $null
        for ($attempt = 0; $attempt -lt 20; $attempt++) {
            $hostKey = & wsl.exe --distribution $distro --user root --exec sh -c 'test ! -s /var/lib/visor-ssh-ci/ssh_host_ed25519.pub || cat /var/lib/visor-ssh-ci/ssh_host_ed25519.pub'
            if ($LASTEXITCODE -eq 0 -and $hostKey) { break }
            if ($owner.HasExited) { throw 'The enrolled host exited before SSH was ready' }
            Start-Sleep -Milliseconds 500
        }
        if ($hostKey -notmatch '^ssh-ed25519 ') { throw 'Installed SSH host key missing' }
        $knownHosts = Join-Path $fixture 'known_hosts'
        "[127.0.0.1]:22222 $hostKey" | Set-Content -LiteralPath $knownHosts -Encoding ascii
        $sshConfig = Join-Path $fixture 'ssh_config'
        @(
            "Host $remoteName", '    HostName 127.0.0.1', '    Port 22222', '    User visorci',
            '    BatchMode yes', '    IdentitiesOnly yes', '    StrictHostKeyChecking yes',
            ('    IdentityFile "{0}"' -f ($identity -replace '\\', '/')),
            ('    UserKnownHostsFile "{0}"' -f ($knownHosts -replace '\\', '/'))
        ) | Set-Content -LiteralPath $sshConfig -Encoding ascii
        $connected = $false
        for ($attempt = 0; $attempt -lt 15; $attempt++) {
            $guestDistro = & ssh.exe -F $sshConfig -o ConnectTimeout=4 $remoteName printenv WSL_DISTRO_NAME 2>$null
            if ($LASTEXITCODE -eq 0 -and $guestDistro -eq $distro) {
                $connected = $true
                break
            }
            Start-Sleep -Seconds 1
        }
        if (!$connected) { throw 'The fixture SSH peer did not attest to the installed distribution' }
    }

    $codeVersion = (Get-Content -LiteralPath 'ci/wsl/vscode-version' -Raw).Trim()
    $zip = Join-Path $fixture 'vscode.zip'
    $url = "https://update.code.visualstudio.com/$codeVersion/win32-x64-archive/stable"
    & curl.exe --proto '=https' --proto-redir '=https' --fail --silent --show-error --location --retry 3 --max-time 180 $url --output $zip
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
    $settings = @{
        'workbench.startupEditor' = 'none'
        'editor.accessibilitySupport' = 'on'
        'security.workspace.trust.startupPrompt' = 'always'
        'extensions.autoUpdate' = $false
        'update.mode' = 'none'
        'telemetry.telemetryLevel' = 'off'
        'window.dialogStyle' = 'custom'
    }
    if ($RemoteSsh) {
        $settings['remote.SSH.configFile'] = $sshConfig
        $settings['remote.SSH.showLoginTerminal'] = $false
        $settings['remote.SSH.useLocalServer'] = $false
    }
    $settings | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $userData 'User/settings.json') -Encoding utf8
    $codeCli = Join-Path $codeDir 'bin/code.cmd'
    $remoteExtension = if ($RemoteSsh) { 'ms-vscode-remote.remote-ssh' } else { 'ms-vscode-remote.remote-wsl' }
    & $codeCli --user-data-dir $userData --extensions-dir $extensions --install-extension $remoteExtension --force
    if ($LASTEXITCODE -ne 0) { throw "VS Code remote extension installation failed for $remoteExtension" }
    & $codeCli --user-data-dir $userData --extensions-dir $extensions --list-extensions --show-versions

    $listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
    $listener.Start()
    $port = $listener.LocalEndpoint.Port
    $listener.Stop()
    $codeProcess = Start-Process -FilePath $executable -PassThru -ArgumentList @(
        ('--user-data-dir="{0}"' -f $userData), ('--extensions-dir="{0}"' -f $extensions),
        '--remote-debugging-address=127.0.0.1', "--remote-debugging-port=$port",
        '--skip-welcome', '--skip-release-notes', '--locale=en-US', '--disable-gpu',
        "--folder-uri=vscode-remote://${transport}+$remoteName$workspace"
    )
    $editorArgs = @($port, $remoteName, $workspace, $case.cliTask, $case.marker)
    if ($RemoteSsh) { $editorArgs += $case.device }
    $result = & node ci/wsl/editor.mjs @editorArgs
    $result | Write-Host
    if ($LASTEXITCODE -ne 0) { throw "The VS Code workspace check failed for $transport" }
    $task = $result | Where-Object { $_ -match '^VISOR_EDITOR_TASK=([a-f0-9-]{36})$' }
    if (@($task).Count -ne 1) { throw 'VS Code task ID missing' }
    $taskId = $task -replace '^VISOR_EDITOR_TASK=', ''
    if ($RemoteSsh) {
        & ssh.exe -F $sshConfig $remoteName /usr/bin/python3 /opt/visor-wsl/task.py verify $taskId
        if ($LASTEXITCODE -ne 0) { throw 'The SSH CLI could not read the editor-started task' }
        & wsl.exe --distribution $distro --user root --exec touch /var/lib/visor-ssh-ci/revoked
        if ($LASTEXITCODE -ne 0) { throw 'The fixture authority did not revoke the assignment' }
        & ssh.exe -F $sshConfig $remoteName /usr/bin/python3 /opt/visor-wsl/task.py verify-revoked $taskId
        if ($LASTEXITCODE -ne 0) { throw 'The revoked task did not pass the SSH CLI refusal check' }
    } else {
        Invoke-WSL /usr/bin/python3 /opt/visor-wsl/task.py verify $taskId
    }
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
