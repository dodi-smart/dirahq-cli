<#
.SYNOPSIS
    dira installer for Windows (https://dirahq.sh)

.DESCRIPTION
    irm https://dirahq.sh/install.ps1 | iex
    irm https://dirahq.sh/install.ps1 | iex -ArgumentList '-Channel','prerelease'   # NOTE: iex ignores
        -ArgumentList; see below for how to actually pass flags through a piped install.

    `irm URL | iex` has no way to forward command-line flags -- iex just executes the
    downloaded text, it does not parse an argument list for it. To pass flags, either
    download the file first, or wrap it in a scriptblock and invoke that directly:

        & ([scriptblock]::Create((irm https://dirahq.sh/install.ps1))) -Channel prerelease

    Or download the script and run it directly (also the only way to use a restricted
    ExecutionPolicy without changing it machine-wide -- `irm | iex` always bypasses policy,
    a `.ps1` file on disk does not):

        powershell -ExecutionPolicy Bypass -File install.ps1 [FLAGS]

.PARAMETER Version
    Install this exact version instead of resolving one. Same as DIRA_VERSION. Default: latest.

.PARAMETER Channel
    stable | prerelease. Same as DIRA_CHANNEL. Default: stable.

.PARAMETER Prerelease
    Shorthand for -Channel prerelease.

.PARAMETER BinDir
    Where to install dira.exe + dirad.exe. Same as DIRA_BIN_DIR. Default: $env:USERPROFILE\.local\bin.

.PARAMETER Target
    Override target-triple detection. Same as DIRA_TARGET.

.PARAMETER Daemon
    Start dirad after installing. Same as DIRA_START_DAEMON=1.

.PARAMETER Service
    Also register dirad as a scheduled task (`dira daemon install`). Same as DIRA_INSTALL_SERVICE=1.

.PARAMETER NoDaemon
    Never start, restart, or install the daemon -- even if one is already running.

.PARAMETER NoInteractive
    Never prompt, even when a console is attached. Same as DIRA_NO_INTERACTIVE=1.

.PARAMETER Force
    Overwrite a dev-build symlink; skip the -Uninstall confirmation.

.PARAMETER Uninstall
    Remove dira + dirad (never touches config or data -- see `dira nuke`).

.PARAMETER Help
    Show usage and exit.

.NOTES
    Environment variables (flags always beat their matching environment variable):
        DIRA_VERSION, DIRA_CHANNEL, DIRA_BIN_DIR, DIRA_TARGET, DIRA_REPO, DIRA_API_URL,
        DIRA_DOWNLOAD_URL, GH_TOKEN / GITHUB_TOKEN (GH_TOKEN wins if both are set),
        DIRA_START_DAEMON, DIRA_INSTALL_SERVICE, DIRA_DEBUG.

    Every download is sha256-verified before anything is installed -- there is no
    -SkipVerify escape hatch. See docs/install.md for manual checksum verification and
    troubleshooting.

    PowerShell 5.1 is the compatibility floor (the default host for `irm | iex`): no `??`,
    no ternary `?:`, no `ConvertFrom-Json -AsHashtable`. This script never calls
    `Set-ExecutionPolicy` -- `irm | iex` already bypasses policy for itself, and a
    file-based run should use `-ExecutionPolicy Bypass` rather than a persistent change.

    Truncation safety, same discipline as install.sh: apart from the `param()` block below
    (a declaration -- it reads a few environment variables for its defaults but calls
    nothing and performs no I/O) and the single `$ErrorActionPreference` assignment,
    everything in this file is a function definition. Nothing is CALLED until the very last
    line, the single `Invoke-Main` call. Do not add a top-level statement with side effects
    anywhere else in this file.

    That last line passes every parameter explicitly rather than splatting
    `@PSBoundParameters`, which would look tidier and be wrong: `$PSBoundParameters` holds
    only the arguments the caller actually passed, so splatting it would silently drop the
    DIRA_VERSION / DIRA_CHANNEL / DIRA_BIN_DIR defaults the `param()` block below resolves
    from the environment. Adding a parameter means adding it to that call too.

    Errors never call `exit` from inside a function: under `irm | iex` there is no child
    process boundary, so `exit` would close the caller's entire PowerShell session, not just
    this script. Every error path below throws a terminating error instead. Running as a
    file (`-File install.ps1`), an uncaught terminating error already makes powershell.exe
    exit non-zero on its own -- no explicit `exit` needed. Running via `iex`, the same
    `throw` is reported as a normal (non-fatal-to-the-host) error in the caller's session.

    The corollary is a contract worth stating outright: a successful run must never leave a
    non-zero $LASTEXITCODE behind. Callers act on it -- GitHub Actions ends every
    `shell: powershell` step with `exit $LASTEXITCODE` -- so an ignored failure from one of
    the best-effort `dira daemon ...` calls below would silently become the caller's exit
    status. `Invoke-BestEffort` is the only thing in this file that runs a native command,
    and resetting $LASTEXITCODE is part of its job.
#>

[CmdletBinding()]
param(
    [string]$Version = $(if ($env:DIRA_VERSION) { $env:DIRA_VERSION } else { 'latest' }),
    [string]$Channel = $(if ($env:DIRA_CHANNEL) { $env:DIRA_CHANNEL } else { 'stable' }),
    [switch]$Prerelease,
    [string]$BinDir = $env:DIRA_BIN_DIR,
    [string]$Target = $env:DIRA_TARGET,
    [switch]$Daemon,
    [switch]$Service,
    [switch]$NoDaemon,
    [switch]$NoInteractive,
    [switch]$Force,
    [switch]$Uninstall,
    [Alias('h')]
    [switch]$Help
)

# Mirrors install.sh's `set -eu`: a shell-option-equivalent assignment, not a call, and it
# touches nothing outside this process. Preference variables flow down the call stack, so
# setting it once here is equivalent to setting it in every function.
$ErrorActionPreference = 'Stop'

# ---------------------------------------------------------------------------
# output helpers
# ---------------------------------------------------------------------------

function Write-Info {
    param([string]$Message)
    [Console]::Error.WriteLine($Message)
}

# Is this PowerShell session running elevated (as Administrator)?
#
# A daemon inherits the elevation of whatever started it, and an elevated dirad
# creates a control channel that ordinary `dira` commands and harness hooks
# cannot open -- which is a silent, days-long capture outage, not a visible
# error. `irm ... | iex` from an Administrator prompt is a completely normal
# instinct, so the installer must not quietly hand the user that state.
function Test-Elevated {
    try {
        $id = [Security.Principal.WindowsIdentity]::GetCurrent()
        return (New-Object Security.Principal.WindowsPrincipal($id)).IsInRole(
            [Security.Principal.WindowsBuiltInRole]::Administrator)
    } catch {
        # Fail open: never block an install on a failed probe.
        return $false
    }
}

# Whether we may ask the user a question: a console has to exist to show it and
# to answer it, and -NoInteractive/DIRA_NO_INTERACTIVE opts out.
#
# The unix twin tests /dev/tty; the Windows equivalent is "is either standard
# stream redirected". Both ends matter. Redirected input means Read-Host would
# consume piped data (or hit EOF and throw); redirected output means the
# question is written somewhere nobody is looking, leaving the run apparently
# hung on an invisible prompt.
#
# `$Host.UI` is additionally probed because a non-interactive host (an ISE-less
# runspace, some CI PowerShell hosts) can have unredirected streams and still
# have no working Read-Host.
function Test-CanPrompt {
    param([switch]$NoInteractive)
    if ($NoInteractive) { return $false }
    try {
        if ([Console]::IsInputRedirected -or [Console]::IsOutputRedirected) { return $false }
        if ($null -eq $Host -or $null -eq $Host.UI) { return $false }
        return $true
    } catch {
        # Fail closed, unlike Test-Elevated: a failed probe here means we do not
        # know whether anyone can answer, and hanging an unattended install on a
        # prompt is worse than skipping an optional step.
        return $false
    }
}

function Write-Warn {
    param([string]$Message)
    [Console]::Error.WriteLine("warning: $Message")
}

# Write-Err prints to stderr, then throws a terminating error carrying the same message --
# see the truncation-safety note above for why this never calls `exit` directly.
function Write-Err {
    param([string]$Message)
    [Console]::Error.WriteLine("error: $Message")
    throw $Message
}

function Write-DebugLog {
    param([string]$Message)
    if ($script:DiraDebug -eq '1') {
        [Console]::Error.WriteLine("debug: $Message")
    }
}

# ---------------------------------------------------------------------------
# best-effort native invocation
# ---------------------------------------------------------------------------

# Runs `& $Exe @Arguments` for its side effect only, discards its output, and returns its
# exit code (-1 if it could not be run at all). install.sh's `|| true`, with two Windows
# PowerShell 5.1 hazards that `|| true` never has to think about:
#
#   1. A native command's stderr becomes *error records* as soon as it is redirected, and
#      under this file's `$ErrorActionPreference = 'Stop'` those records are TERMINATING --
#      even when the command itself exited 0. `dira daemon uninstall` shells out to
#      `schtasks` and `reg`, which print "ERROR: ..." whenever there is nothing to remove,
#      so the plain `& $exe ... *> $null` this replaces always threw on a clean machine and
#      its exit code was never actually read. Dropping to 'Continue' for the duration of the
#      call is the fix; the assignment is function-scoped and cannot leak past the return.
#
#   2. $LASTEXITCODE lives in the GLOBAL scope -- a plain assignment here would only create
#      a useless local copy. It has to be reset, because every caller below has *chosen* to
#      ignore this command's failure and must not silently re-export it: install.ps1 reports
#      failure by throwing, never through an exit code (see the truncation-safety note at the
#      top of this file), while GitHub Actions ends every `shell: powershell` step with
#      `exit $LASTEXITCODE` -- which is how a fully successful `install.ps1 -Uninstall`
#      failed a release smoke leg. `Set-Variable -Scope Global` rather than
#      `$global:LASTEXITCODE` keeps PSScriptAnalyzer's PSAvoidGlobalVars rule quiet, which
#      CI's powershell-lint job enforces at Warning severity with -EnableExit.
function Invoke-BestEffort {
    param([string]$Exe, [string[]]$Arguments)
    $code = -1
    $ErrorActionPreference = 'Continue'
    try {
        & $Exe @Arguments 2>&1 | Out-Null
        $code = $LASTEXITCODE
    } catch {
        Write-DebugLog "best-effort '$Exe $($Arguments -join ' ')' failed (ignored): $($_.Exception.Message)"
    }
    Set-Variable -Name LASTEXITCODE -Value 0 -Scope Global
    return $code
}

# ---------------------------------------------------------------------------
# usage
# ---------------------------------------------------------------------------

function Show-Usage {
    @'
dira installer (Windows)

USAGE:
    irm https://dirahq.sh/install.ps1 | iex
    & ([scriptblock]::Create((irm https://dirahq.sh/install.ps1))) [FLAGS]

    Or download the script and run it directly -- also how to use a restricted
    ExecutionPolicy without a persistent change (`irm | iex` already bypasses policy for
    itself; a saved file does not):

        powershell -ExecutionPolicy Bypass -File install.ps1 [FLAGS]

FLAGS:
    -Version <VERSION>   Install this exact version instead of resolving one (default: latest)
    -Channel <CHANNEL>   stable | prerelease                                  (default: stable)
    -Prerelease           Shorthand for -Channel prerelease
    -BinDir <DIR>          Where to install dira.exe + dirad.exe
                            (default: $env:USERPROFILE\.local\bin)
    -Target <TRIPLE>        Override target-triple detection
    -Daemon                  Start dirad after installing
    -Service                  Also register dirad as a scheduled task (dira daemon install)
    -NoDaemon                  Never start, restart, or install the daemon -- even if
                                one is already running
    -NoInteractive              Never prompt, even on a console
    -Force                      Overwrite a dev-build symlink; skip the -Uninstall
                                  confirmation
    -Uninstall                    Remove dira + dirad (never touches config or data --
                                    see `dira nuke`)
    -Help                          Show this help and exit

ENVIRONMENT:
    DIRA_VERSION              Same as -Version                          (default: latest)
    DIRA_CHANNEL                Same as -Channel                          (default: stable)
    DIRA_BIN_DIR                  Same as -BinDir
    DIRA_TARGET                    Same as -Target
    DIRA_REPO                       GitHub repo to install from            (default: dodi-smart/dirahq-cli)
    DIRA_API_URL                      GitHub API base URL                     (default: https://api.github.com)
    DIRA_DOWNLOAD_URL                   Override the release-asset base URL (air-gapped / local installs)
    GITHUB_TOKEN / GH_TOKEN                Bearer token for the private-repo asset path
                                             (GH_TOKEN wins if both are set)
    DIRA_START_DAEMON                        Same as -Daemon        (set to 1)
    DIRA_INSTALL_SERVICE                       Same as -Service        (set to 1)
    DIRA_NO_INTERACTIVE                          Same as -NoInteractive (set to 1)
    DIRA_DEBUG                                   Verbose debug output on stderr (set to 1)

    Flags always beat their matching environment variable.

Every download is sha256-verified before anything is installed -- there is no
-SkipVerify escape hatch. See docs/install.md for manual checksum verification,
air-gapped installs, and troubleshooting.
'@ -split "`n" | ForEach-Object { [Console]::Error.WriteLine($_) }
}

# ---------------------------------------------------------------------------
# preflight
# ---------------------------------------------------------------------------

function Test-Preflight {
    try {
        [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    } catch {
        # Some very old .NET Framework patch levels don't know the Tls12 enum member by
        # name; most current defaults already negotiate it, and a download that genuinely
        # can't will fail loudly and specifically later rather than silently here.
        Write-DebugLog "could not force TLS 1.2: $($_.Exception.Message)"
    }
    foreach ($cmd in 'Expand-Archive', 'Get-FileHash') {
        if (-not (Get-Command $cmd -ErrorAction SilentlyContinue)) {
            Write-Err "missing required PowerShell cmdlet: $cmd (needs PowerShell 5.1+ with the Microsoft.PowerShell.Archive and Microsoft.PowerShell.Utility modules)"
        }
    }
}

# ---------------------------------------------------------------------------
# OS / arch / target detection
# ---------------------------------------------------------------------------

function Get-Target {
    # $IsLinux / $IsMacOS only exist on PowerShell 6+ (Windows PowerShell 5.1 is Windows-only
    # and never defines them, so this reads as $null/false there -- exactly what we want).
    if ($IsLinux -or $IsMacOS) {
        Write-Err "this is the Windows installer -- on Linux/macOS (including WSL) use: curl -fsSL https://dirahq.sh/install | sh"
    }
    # PROCESSOR_ARCHITEW6432 is set when a 32-bit PowerShell host is running on a 64-bit
    # OS -- it names the *real* OS architecture, which PROCESSOR_ARCHITECTURE alone would
    # under-report as x86 in that case.
    $arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
    switch ($arch) {
        'AMD64' { return 'x86_64-pc-windows-msvc' }
        'ARM64' { return 'aarch64-pc-windows-msvc' }
        default { Write-Err "unsupported Windows architecture: $arch (supported: AMD64, ARM64)" }
    }
}

# ---------------------------------------------------------------------------
# semver compare (ported from install.sh's _semver_gt / _pick_highest_tag --
# preserves the same prerelease ordering semantics, SemVer 2.0 section 11)
# ---------------------------------------------------------------------------

function Test-IsNumericIdentifier {
    param([string]$Value)
    return $Value -match '^[0-9]+$'
}

function Compare-PrereleaseGreater {
    param([string]$A, [string]$B)
    $aParts = if ($A) { $A -split '\.' } else { @() }
    $bParts = if ($B) { $B -split '\.' } else { @() }
    $len = [Math]::Max($aParts.Count, $bParts.Count)
    for ($i = 0; $i -lt $len; $i++) {
        $af = if ($i -lt $aParts.Count) { $aParts[$i] } else { $null }
        $bf = if ($i -lt $bParts.Count) { $bParts[$i] } else { $null }
        if ($null -eq $af -and $null -eq $bf) { return $false }
        if ($null -eq $af) { return $false }
        if ($null -eq $bf) { return $true }
        $aNum = Test-IsNumericIdentifier $af
        $bNum = Test-IsNumericIdentifier $bf
        if ($aNum -and $bNum) {
            $an = [int64]$af
            $bn = [int64]$bf
            if ($an -gt $bn) { return $true }
            if ($an -lt $bn) { return $false }
        } else {
            if ($aNum) { return $false }
            if ($bNum) { return $true }
            if ($af -ne $bf) {
                return ([string]::CompareOrdinal($af, $bf) -gt 0)
            }
        }
    }
    return $false
}

function Split-SemverCore {
    param([string]$Ver)
    $idx = $Ver.IndexOf('-')
    if ($idx -ge 0) {
        return @($Ver.Substring(0, $idx), $Ver.Substring($idx + 1))
    }
    return @($Ver, '')
}

# "v1.2.3" -> "1.2.3"; anything without the prefix passes through unchanged. Release tags
# carry the "v", every version comparison and asset name below wants it gone, and the
# conversion is needed at seven call sites -- so it lives here once.
function Get-BareVersion {
    param([string]$Tag)
    if ($Tag.StartsWith('v')) { return $Tag.Substring(1) }
    return $Tag
}

# True if bare version A ("MAJOR.MINOR.PATCH[-PRE]", no leading "v") is greater than B. A
# release outranks a prerelease with the same core version.
function Compare-SemverGreater {
    param([string]$A, [string]$B)
    $aSplit = Split-SemverCore $A
    $bSplit = Split-SemverCore $B
    $aParts = $aSplit[0] -split '\.'
    $bParts = $bSplit[0] -split '\.'
    $aMaj = if ($aParts.Count -gt 0) { [int]$aParts[0] } else { 0 }
    $aMin = if ($aParts.Count -gt 1) { [int]$aParts[1] } else { 0 }
    $aPat = if ($aParts.Count -gt 2) { [int]$aParts[2] } else { 0 }
    $bMaj = if ($bParts.Count -gt 0) { [int]$bParts[0] } else { 0 }
    $bMin = if ($bParts.Count -gt 1) { [int]$bParts[1] } else { 0 }
    $bPat = if ($bParts.Count -gt 2) { [int]$bParts[2] } else { 0 }

    if ($aMaj -ne $bMaj) { return $aMaj -gt $bMaj }
    if ($aMin -ne $bMin) { return $aMin -gt $bMin }
    if ($aPat -ne $bPat) { return $aPat -gt $bPat }

    $aPre = $aSplit[1]
    $bPre = $bSplit[1]
    if (-not $aPre -and -not $bPre) { return $false }
    if (-not $aPre) { return $true }
    if (-not $bPre) { return $false }
    return Compare-PrereleaseGreater -A $aPre -B $bPre
}

# Picks the highest "vX.Y.Z[-pre]" tag out of an array.
function Select-HighestTag {
    param([string[]]$Tags)
    $best = $null
    $bestVer = $null
    foreach ($line in $Tags) {
        if (-not $line) { continue }
        $ver = Get-BareVersion $line
        if ($null -eq $best) {
            $best = $line
            $bestVer = $ver
            continue
        }
        if (Compare-SemverGreater -A $ver -B $bestVer) {
            $best = $line
            $bestVer = $ver
        }
    }
    return $best
}

# ---------------------------------------------------------------------------
# HTTP: GitHub API (JSON) + generic file download
# ---------------------------------------------------------------------------

# Invoke-GhApi <path> -- GET ${ApiUrl}<path>, return the deserialized JSON body.
# PowerShell parses JSON natively via Invoke-RestMethod, so unlike install.sh (which only
# reaches for jq on the authenticated path to avoid requiring it for every plain install)
# there is no unauthenticated/no-JSON split here -- every call goes through this one path.
function Invoke-GhApi {
    param(
        [string]$Path,
        [string]$ApiUrl,
        [string]$Token
    )
    $uri = "$ApiUrl$Path"
    $headers = @{
        'Accept'                = 'application/vnd.github+json'
        'X-GitHub-Api-Version'  = '2022-11-28'
    }
    if ($Token) {
        $headers['Authorization'] = "Bearer $Token"
    }
    try {
        $script:LastGhApiStatus = 0
        return Invoke-RestMethod -Uri $uri -Headers $headers -UseBasicParsing
    } catch {
        # Record the status code for the caller BEFORE throwing, so a 401 can be
        # recovered from rather than parsed back out of an error string. 5.1 and
        # 7+ expose the response differently, hence the two probes.
        $script:LastGhApiStatus = Get-HttpStatusCode -ErrorRecord $_
        if ($script:LastGhApiStatus -eq 401) {
            # Deliberately not fatal here -- Invoke-Main retries anonymously. See
            # the note there for why a rejected token must never be terminal.
            throw "unauthorized"
        }
        Write-Err "GitHub API request failed: $uri ($($_.Exception.Message))"
    }
}

# The HTTP status behind a failed Invoke-RestMethod/Invoke-WebRequest, or 0 if it
# wasn't an HTTP error at all (DNS, TLS, connection refused). Windows PowerShell
# 5.1 throws a WebException carrying an HttpWebResponse, while PowerShell 7+
# throws HttpResponseException with a StatusCode on the record itself -- probe
# both rather than assuming a host.
function Get-HttpStatusCode {
    param($ErrorRecord)
    $response = $ErrorRecord.Exception.Response
    if ($response -and $response.StatusCode) {
        return [int]$response.StatusCode
    }
    if ($ErrorRecord.Exception.StatusCode) {
        return [int]$ErrorRecord.Exception.StatusCode
    }
    return 0
}

# Save-Download <url> <out-file> -- unauthenticated download (public asset URLs).
function Save-Download {
    param([string]$Url, [string]$OutFile)
    if ($Url -like 'file://*') {
        # Invoke-WebRequest has no file:// support on any PowerShell version -- its
        # underlying HttpClient rejects the scheme outright. Read the local path directly
        # instead, so DIRA_DOWNLOAD_URL=file://... (documented for air-gapped installs,
        # and free on install.sh's side via curl/wget's native file:// support) still
        # works here.
        $localPath = ([uri]$Url).LocalPath
        try {
            Copy-Item -Path $localPath -Destination $OutFile -Force -ErrorAction Stop
            return
        } catch {
            Write-Err "download failed: $Url ($($_.Exception.Message))"
        }
    }
    $attempts = 3
    for ($i = 1; $i -le $attempts; $i++) {
        try {
            Invoke-WebRequest -Uri $Url -OutFile $OutFile -UseBasicParsing -ErrorAction Stop
            return
        } catch {
            if ($i -eq $attempts) {
                Write-Err "download failed: $Url ($($_.Exception.Message))"
            }
            Write-DebugLog "download attempt $i failed for $Url -- retrying: $($_.Exception.Message)"
            Start-Sleep -Seconds ([Math]::Min(2 * $i, 5))
        }
    }
}

# Save-AssetById <asset-id> <out-file> -- authenticated download by asset id.
# browser_download_url is not bearer-fetchable on a private repo, so the authenticated
# path must hit /repos/<repo>/releases/assets/<id> with Accept: application/octet-stream
# instead -- same reasoning as install.sh's _dl_asset.
function Save-AssetById {
    param([string]$ApiUrl, [string]$Repo, [string]$AssetId, [string]$Token, [string]$OutFile)
    $uri = "$ApiUrl/repos/$Repo/releases/assets/$AssetId"
    $headers = @{
        'Accept'               = 'application/octet-stream'
        'X-GitHub-Api-Version' = '2022-11-28'
        'Authorization'        = "Bearer $Token"
    }
    $attempts = 3
    for ($i = 1; $i -le $attempts; $i++) {
        try {
            Invoke-WebRequest -Uri $uri -Headers $headers -OutFile $OutFile -UseBasicParsing -ErrorAction Stop
            return
        } catch {
            if ($i -eq $attempts) {
                Write-Err "authenticated download failed for asset $AssetId ($($_.Exception.Message))"
            }
            Write-DebugLog "authenticated download attempt $i failed for asset $AssetId -- retrying: $($_.Exception.Message)"
            Start-Sleep -Seconds ([Math]::Min(2 * $i, 5))
        }
    }
}

# ---------------------------------------------------------------------------
# version + asset resolution
# ---------------------------------------------------------------------------

# Unauthenticated path: builds the download URLs directly rather than trusting
# browser_download_url, so DIRA_DOWNLOAD_URL keeps working as an override -- this is the
# path every real end user takes against a public repo.
function Resolve-ReleaseUnauthenticated {
    param(
        [string]$VersionPin,
        [string]$Channel,
        [string]$Repo,
        [string]$ApiUrl,
        [string]$DownloadUrl,
        [string]$Target
    )
    if ($VersionPin -ne 'latest') {
        $version = Get-BareVersion $VersionPin
        $tag = "v$version"
    } elseif ($Channel -eq 'prerelease') {
        $body = Invoke-GhApi -Path "/repos/$Repo/releases?per_page=30" -ApiUrl $ApiUrl -Token ''
        $tags = @($body | ForEach-Object { $_.tag_name })
        if ($tags.Count -eq 0) { Write-Err "no releases found for $Repo" }
        $tag = Select-HighestTag -Tags $tags
        if (-not $tag) { Write-Err "could not determine the newest prerelease tag" }
        $version = Get-BareVersion $tag
    } else {
        $body = Invoke-GhApi -Path "/repos/$Repo/releases/latest" -ApiUrl $ApiUrl -Token ''
        $tag = $body.tag_name
        if (-not $tag) {
            Write-Err "failed to resolve the latest stable release for $Repo (no stable release yet? try -Channel prerelease. GitHub also rate-limits anonymous requests to 60/hr per IP -- set GITHUB_TOKEN/GH_TOKEN to raise it)"
        }
        $version = Get-BareVersion $tag
    }

    $archiveName = "dira-$version-$Target.zip"
    $shaName = "dira-$version-$Target.sha256"
    $base = if ($DownloadUrl) { $DownloadUrl.TrimEnd('/') } else { "https://github.com/$Repo/releases/download/$tag" }
    return [PSCustomObject]@{
        Version     = $version
        Tag         = $tag
        ArchiveName = $archiveName
        ShaName     = $shaName
        ArchiveUrl  = "$base/$archiveName"
        ShaUrl      = "$base/$shaName"
    }
}

# Authenticated path: requires a lookup of the asset id (not just its name) because the
# download step below must hit the octet-stream asset endpoint, not browser_download_url.
function Resolve-ReleaseAuthenticated {
    param(
        [string]$VersionPin,
        [string]$Channel,
        [string]$Repo,
        [string]$ApiUrl,
        [string]$Token,
        [string]$Target
    )
    if ($VersionPin -ne 'latest') {
        $version = Get-BareVersion $VersionPin
        $tag = "v$version"
        $body = Invoke-GhApi -Path "/repos/$Repo/releases/tags/$tag" -ApiUrl $ApiUrl -Token $Token
        $assets = $body.assets
    } elseif ($Channel -eq 'prerelease') {
        $body = Invoke-GhApi -Path "/repos/$Repo/releases?per_page=30" -ApiUrl $ApiUrl -Token $Token
        $releases = @($body | Where-Object { $_.draft -eq $false })
        if ($releases.Count -eq 0) { Write-Err "no releases found for $Repo" }
        $tags = @($releases | ForEach-Object { $_.tag_name })
        $tag = Select-HighestTag -Tags $tags
        if (-not $tag) { Write-Err "could not determine the newest prerelease tag" }
        $version = Get-BareVersion $tag
        $release = $releases | Where-Object { $_.tag_name -eq $tag } | Select-Object -First 1
        $assets = $release.assets
    } else {
        $body = Invoke-GhApi -Path "/repos/$Repo/releases/latest" -ApiUrl $ApiUrl -Token $Token
        $tag = $body.tag_name
        if (-not $tag) {
            Write-Err "failed to resolve the latest stable release for $Repo (authenticated) -- does a stable release exist yet? try -Channel prerelease"
        }
        $version = Get-BareVersion $tag
        $assets = $body.assets
    }

    $archiveName = "dira-$version-$Target.zip"
    $shaName = "dira-$version-$Target.sha256"
    $archiveAsset = $assets | Where-Object { $_.name -eq $archiveName } | Select-Object -First 1
    $shaAsset = $assets | Where-Object { $_.name -eq $shaName } | Select-Object -First 1
    if (-not $archiveAsset) { Write-Err "release $tag has no asset named $archiveName -- was it built for this target?" }
    if (-not $shaAsset) { Write-Err "release $tag has no checksum asset named $shaName" }

    return [PSCustomObject]@{
        Version     = $version
        Tag         = $tag
        ArchiveName = $archiveName
        ShaName     = $shaName
        ArchiveId   = $archiveAsset.id
        ShaId       = $shaAsset.id
    }
}

# ---------------------------------------------------------------------------
# checksum verification (mandatory -- there is no -SkipVerify)
# ---------------------------------------------------------------------------

# The .sha256 asset holds one line per asset built in that release job (raw sha256sum
# output), so this must pick the line whose filename field matches our archive -- not just
# read the first line. Mirrors install.sh's _extract_expected_digest.
function Test-Checksum {
    param([string]$ShaFile, [string]$WantName, [string]$FilePath)
    $expected = $null
    foreach ($line in Get-Content -Path $ShaFile) {
        if (-not $line) { continue }
        $parts = $line -split '\s+', 2
        if ($parts.Count -lt 2) { continue }
        # A leading '*' on the filename marks sha256sum "binary mode" -- strip it.
        $fname = $parts[1].TrimStart('*').Trim()
        if ($fname.ToLowerInvariant() -eq $WantName.ToLowerInvariant()) {
            $expected = $parts[0]
            break
        }
    }
    if (-not $expected) {
        Write-Err "checksum file has no entry for $WantName"
    }
    $actual = (Get-FileHash -Path $FilePath -Algorithm SHA256).Hash
    $expectedLower = $expected.ToLowerInvariant()
    $actualLower = $actual.ToLowerInvariant()
    if ($expectedLower -ne $actualLower) {
        Write-Err "checksum mismatch for $WantName -- expected $expectedLower, got $actualLower -- download is corrupt or tampered, aborting"
    }
    Write-DebugLog "checksum OK for $WantName"
}

# ---------------------------------------------------------------------------
# install-time helpers
# ---------------------------------------------------------------------------

# True whether $Path exists as a normal file OR as a (possibly broken) reparse point --
# Test-Path alone follows links and reports $false for a broken one, which would hide a
# dev symlink whose target has since been cleaned (`cargo clean`, a switched branch, etc).
function Test-ExistsOrLink {
    param([string]$Path)
    if (Test-Path -Path $Path) { return $true }
    return $null -ne (Get-Item -Path $Path -Force -ErrorAction SilentlyContinue)
}

# True if $Path is a symlink/reparse point into a dev build (target\release or
# target\debug) -- re-running this installer over one of those must refuse unless -Force.
# D-0004's Windows analog.
function Test-DevInstall {
    param([string]$Path)
    $item = Get-Item -Path $Path -Force -ErrorAction SilentlyContinue
    if (-not $item -or -not $item.LinkType) { return $false }
    $targetStr = @($item.Target) -join ';'
    return ($targetStr -match '[\\/]target[\\/](release|debug)[\\/]')
}

# Move-IntoPlace <name> <source-file> <dest-dir> -- stages into a same-directory temp file,
# then moves it onto the destination. D-0003's Windows analog: renaming a *running* exe
# aside is legal on Windows (the process keeps its open handle by file identity, not path),
# so on the rare failure where the direct move is blocked, move the current destination
# aside to a `.old` name first, then move the staged file into the now-free name.
function Move-IntoPlace {
    param([string]$Name, [string]$SourceFile, [string]$DestDir)
    $dest = Join-Path $DestDir $Name
    $staging = Join-Path $DestDir ".$Name.new.$PID"
    Copy-Item -Path $SourceFile -Destination $staging -Force

    try {
        Move-Item -Path $staging -Destination $dest -Force -ErrorAction Stop
        return
    } catch {
        if (-not (Test-Path -Path $dest)) { throw }
        Write-DebugLog "direct move onto $dest failed ($($_.Exception.Message)) -- trying rename-aside"
    }

    # Dotted, exactly like the staging name above and like replace.rs's
    # `.{name}.old.{unique}` -- `dira update`'s own sweep (cleanup_stale_old_files)
    # matches on that leading dot, so an undotted name here would leave a sidecar only
    # a later install.ps1 run could ever clear.
    $old = Join-Path $DestDir ".$Name.old.$PID"
    try {
        Move-Item -Path $dest -Destination $old -Force -ErrorAction Stop
    } catch {
        Remove-Item -Path $staging -Force -ErrorAction SilentlyContinue
        Write-Err "could not replace $dest -- it may be locked by something other than a running dira/dirad: $($_.Exception.Message)"
    }
    # The destination name is now free; the final move can still fail transiently
    # (Defender's on-access scan briefly locks freshly written files -- the Rust
    # updater retries the exact same way, see replace.rs::retry_rename). Retry a
    # few times, and on exhausted failure move the old binary back into place:
    # without the rollback the machine would be left with NO binary at $dest,
    # which is strictly worse than the old version it started with.
    $attempts = 3
    for ($i = 1; $i -le $attempts; $i++) {
        try {
            Move-Item -Path $staging -Destination $dest -Force -ErrorAction Stop
            return
        } catch {
            if ($i -lt $attempts) {
                Write-DebugLog "move onto $dest failed (attempt $i/$attempts): $($_.Exception.Message)"
                Start-Sleep -Milliseconds 100
            } else {
                Move-Item -Path $old -Destination $dest -Force -ErrorAction SilentlyContinue
                Remove-Item -Path $staging -Force -ErrorAction SilentlyContinue
                Write-Err "could not place the new $Name at $dest -- the previous binary was restored: $($_.Exception.Message)"
            }
        }
    }
}

# Best-effort: a `.old` file from a prior run whose owning process is still running stays
# locked -- ignore failures, it clears up once that process exits. Sweeps sidecars left by
# either implementation (this script and `dira update` both write `.{name}.old.{unique}`).
# `-like` rather than `-Filter`: the provider filter is matched by the Win32 wildcard
# engine, whose handling of leading dots and multiple extensions is not worth relying on
# for a cleanup that silently does nothing when it mismatches.
function Clear-StaleOldFile {
    param([string]$DestDir)
    Get-ChildItem -Path $DestDir -File -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -like '*.old.*' } |
        ForEach-Object {
            try {
                Remove-Item -Path $_.FullName -Force -ErrorAction Stop
            } catch {
                Write-DebugLog "leaving stale file in place, still locked: $($_.FullName)"
            }
        }
}

function Install-Binary {
    param([string]$ExtractDir, [string]$DestDir)
    Clear-StaleOldFile -DestDir $DestDir
    # dirad.exe FIRST, then dira.exe -- same order and reasoning as install.sh / the
    # updater's swap (D-0003): dying between the two leaves a new dirad under an old dira,
    # which `dira version` already detects and warns about. The reverse leaves a new CLI
    # silently driving a stale daemon, which looks like success.
    Move-IntoPlace -Name 'dirad.exe' -SourceFile (Join-Path $ExtractDir 'dirad.exe') -DestDir $DestDir
    Move-IntoPlace -Name 'dira.exe' -SourceFile (Join-Path $ExtractDir 'dira.exe') -DestDir $DestDir
}

# Adds $Dir to the *user* PATH -- never machine PATH, never `setx` (silently truncates at
# 1024 chars and has corrupted other tools' PATH entries in the wild in the past). Also
# prepends to the current session's $env:Path so "Next steps" below work immediately.
function Add-UserPath {
    param([string]$Dir)
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $entries = if ($userPath) { $userPath -split ';' } else { @() }
    $already = $entries | Where-Object { $_.TrimEnd('\') -ieq $Dir.TrimEnd('\') }
    if ($already) {
        Write-DebugLog "$Dir is already on the user PATH"
    } else {
        $newPath = if ($userPath) { "$userPath;$Dir" } else { $Dir }
        [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
        Write-Info "`n$Dir was added to your user PATH. Open a new terminal to pick it up."
    }
    if (";$env:Path;" -notlike "*;$Dir;*") {
        $env:Path = "$Dir;$env:Path"
    }
}

# ---------------------------------------------------------------------------
# uninstall (binaries + scheduled task only -- never config or data)
# ---------------------------------------------------------------------------

function Uninstall-Dira {
    param([string]$BinDir, [bool]$Force)
    $diraExe = Join-Path $BinDir 'dira.exe'
    $diradExe = Join-Path $BinDir 'dirad.exe'
    $installed = (Test-ExistsOrLink $diraExe) -or (Test-ExistsOrLink $diradExe)

    if ($installed) {
        if (Test-DevInstall -Path $diraExe) {
            $devTarget = (@((Get-Item -Path $diraExe -Force).Target)) -join ', '
            Write-Err "$diraExe is a symlink into a dev build ($devTarget) -- this installer only manages its own installs. Remove it yourself if that's what you want."
        }

        if (-not $Force) {
            # [string](...) is load-bearing, not decorative: at EOF (no tty, e.g. a
            # non-interactive `-Uninstall` without -Force) Read-Host returns PowerShell's
            # internal "nothing" value, not a real $null -- it satisfies `-eq $null` but
            # `-notmatch` silently treats it as an empty *collection* and returns an empty
            # array instead of a boolean, which is falsy and would fall through to delete
            # with no confirmation at all. Casting to [string] first forces a real empty
            # string, so an unanswered prompt correctly aborts instead of un-confirming.
            # ${BinDir} (braced), not "$BinDir?": a bare "$BinDir?" is parsed as a
            # reference to a variable literally named "BinDir?", which doesn't exist and
            # silently interpolates as empty -- a real and non-obvious PowerShell gotcha.
            $reply = [string](Read-Host "Remove dira and dirad from ${BinDir}? [y/N]")
            if ($reply -notmatch '^(?i:y|yes)$') {
                Write-Info "aborted -- nothing removed."
                return
            }
        }

        if (Test-Path -Path $diraExe) {
            Invoke-BestEffort -Exe $diraExe -Arguments @('daemon', 'stop') | Out-Null
        }
    }

    # Best-effort scheduled-task teardown, independent of whether the binaries are still
    # present -- a stray task from a previous install must still go.
    $tornDown = $false
    if (Test-Path -Path $diraExe) {
        $tornDown = (Invoke-BestEffort -Exe $diraExe -Arguments @('daemon', 'uninstall')) -eq 0
    }
    if (-not $tornDown) {
        try {
            Unregister-ScheduledTask -TaskName 'DiraDaemon' -Confirm:$false -ErrorAction SilentlyContinue
        } catch {
            Write-DebugLog "Unregister-ScheduledTask failed (ignored, best-effort): $($_.Exception.Message)"
        }
        # `dira daemon install` falls back to an HKCU Run key when schtasks needs
        # elevation, so the fallback teardown must sweep BOTH artifacts -- a stale
        # Run key would relaunch a binary this uninstall is about to delete.
        try {
            Remove-ItemProperty -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name 'DiraDaemon' -ErrorAction SilentlyContinue
        } catch {
            Write-DebugLog "Run-key removal failed (ignored, best-effort): $($_.Exception.Message)"
        }
    }

    if (-not $installed) {
        Write-Info "dira is not installed at $BinDir -- nothing to remove."
        return
    }

    Remove-Item -Path $diraExe, $diradExe -Force -ErrorAction SilentlyContinue
    Write-Info "removed dira and dirad from $BinDir"
    Write-Info "`nConfig and data were NOT removed."
    Write-Info "To remove everything, reinstall dira and run 'dira nuke', or delete the config/data directories by hand -- see docs/install.md."
}

# ---------------------------------------------------------------------------
# main
# ---------------------------------------------------------------------------

function Invoke-Main {
    param(
        [string]$Version,
        [string]$Channel,
        [switch]$Prerelease,
        [string]$BinDir,
        [string]$Target,
        [switch]$Daemon,
        [switch]$Service,
        [switch]$NoDaemon,
        [switch]$NoInteractive,
        [switch]$Force,
        [switch]$Uninstall,
        [switch]$Help
    )

    if ($Help) {
        Show-Usage
        return
    }

    # 1/2. defaults + flags: the script param() block already folds the DIRA_* environment
    # fallbacks into each parameter's default value, so an explicit flag beats its env var
    # for free (PowerShell parameter binding always prefers the passed-in argument over the
    # default value expression).
    $effectiveChannel = if ($Prerelease) { 'prerelease' } else { $Channel }
    if ($effectiveChannel -ne 'stable' -and $effectiveChannel -ne 'prerelease') {
        Write-Err "-Channel must be 'stable' or 'prerelease' (got: $effectiveChannel)"
    }

    $startDaemon = $Daemon.IsPresent -or ($env:DIRA_START_DAEMON -eq '1')
    $installService = $Service.IsPresent -or ($env:DIRA_INSTALL_SERVICE -eq '1')
    $noInteractive = $NoInteractive.IsPresent -or ($env:DIRA_NO_INTERACTIVE -eq '1')
    $script:DiraDebug = if ($env:DIRA_DEBUG -eq '1') { '1' } else { '0' }

    # 3. bin dir -- %USERPROFILE%\.local\bin is the Windows peer convention (Claude Code
    # uses exactly this path).
    $resolvedBinDir = if ($BinDir) { $BinDir } else { Join-Path $env:USERPROFILE '.local\bin' }

    # 4. auth token: GH_TOKEN wins if both are set (matches `gh`'s own precedence).
    $repo = if ($env:DIRA_REPO) { $env:DIRA_REPO } else { 'dodi-smart/dirahq-cli' }
    $apiUrl = if ($env:DIRA_API_URL) { $env:DIRA_API_URL } else { 'https://api.github.com' }
    $downloadUrl = $env:DIRA_DOWNLOAD_URL
    $token = if ($env:GH_TOKEN) { $env:GH_TOKEN } elseif ($env:GITHUB_TOKEN) { $env:GITHUB_TOKEN } else { '' }

    Test-Preflight

    if ($Uninstall) {
        Uninstall-Dira -BinDir $resolvedBinDir -Force $Force.IsPresent
        return
    }

    # 5. target
    $resolvedTarget = if ($Target) { $Target } else { Get-Target }
    Write-DebugLog "target: $resolvedTarget"

    # 6. tmp dir + cleanup, installed before the first download.
    $tmp = Join-Path $env:TEMP "dira-install.$([guid]::NewGuid().ToString('N').Substring(0, 8))"
    New-Item -ItemType Directory -Path $tmp -Force | Out-Null
    try {
        # 7. version + asset resolution.
        #
        # A token is an optimization on a public repo, never a requirement: it
        # only lifts GitHub's 60 req/hr anonymous per-IP limit. So a token the
        # API rejects must not be fatal -- drop it and resolve anonymously,
        # which is the path every normal user takes anyway. Without this, any
        # developer with a stale or expired GITHUB_TOKEN/GH_TOKEN exported in
        # their shell -- extremely common, and nothing to do with dira -- got a
        # hard `401 (Unauthorized)` from `irm | iex` and could not install at
        # all, on a repo that needs no credentials.
        #
        # Clearing $token (not just retrying the one call) is the point: it also
        # switches the download below from Save-AssetById, which sends the same
        # rejected bearer, to the plain public asset URLs.
        if ($token) {
            try {
                $release = Resolve-ReleaseAuthenticated -VersionPin $Version -Channel $effectiveChannel -Repo $repo -ApiUrl $apiUrl -Token $token -Target $resolvedTarget
            } catch {
                if ($script:LastGhApiStatus -ne 401) { throw }
                Write-Warn "GITHUB_TOKEN/GH_TOKEN was rejected by GitHub (401) -- ignoring it and continuing anonymously. Unset or replace that token to silence this."
                $token = ''
                $release = Resolve-ReleaseUnauthenticated -VersionPin $Version -Channel $effectiveChannel -Repo $repo -ApiUrl $apiUrl -DownloadUrl $downloadUrl -Target $resolvedTarget
            }
        } else {
            $release = Resolve-ReleaseUnauthenticated -VersionPin $Version -Channel $effectiveChannel -Repo $repo -ApiUrl $apiUrl -DownloadUrl $downloadUrl -Target $resolvedTarget
        }
        Write-Info "installing dira $($release.Version) ($resolvedTarget)"

        # 8. download
        $localArchive = Join-Path $tmp $release.ArchiveName
        $localSha = Join-Path $tmp $release.ShaName
        if ($token) {
            Save-AssetById -ApiUrl $apiUrl -Repo $repo -AssetId $release.ArchiveId -Token $token -OutFile $localArchive
            Save-AssetById -ApiUrl $apiUrl -Repo $repo -AssetId $release.ShaId -Token $token -OutFile $localSha
        } else {
            Save-Download -Url $release.ArchiveUrl -OutFile $localArchive
            Save-Download -Url $release.ShaUrl -OutFile $localSha
        }

        # 9. checksum verification is mandatory.
        Test-Checksum -ShaFile $localSha -WantName $release.ArchiveName -FilePath $localArchive

        # 10. extract; the zip root is flat (dira.exe + dirad.exe, no leading dir).
        $extractDir = Join-Path $tmp 'extract'
        Expand-Archive -Path $localArchive -DestinationPath $extractDir -Force
        $extractedDira = Join-Path $extractDir 'dira.exe'
        $extractedDirad = Join-Path $extractDir 'dirad.exe'
        if (-not (Test-Path -Path $extractedDira)) {
            Write-Err "downloaded archive is missing 'dira.exe' at its root -- packaging layout may have changed (see docs/install.md)"
        }
        if (-not (Test-Path -Path $extractedDirad)) {
            Write-Err "downloaded archive is missing 'dirad.exe' at its root -- packaging layout may have changed (see docs/install.md)"
        }
        if ((Get-Item -Path $extractedDira).Length -eq 0) { Write-Err "downloaded 'dira.exe' binary is empty" }
        if ((Get-Item -Path $extractedDirad).Length -eq 0) { Write-Err "downloaded 'dirad.exe' binary is empty" }

        # Mark-of-the-Web: strip it only after checksum verification has proven the bytes
        # are exactly what the release published.
        Unblock-File -Path $extractedDira, $extractedDirad -ErrorAction SilentlyContinue

        # 11. existing-install checks, before writing anything.
        New-Item -ItemType Directory -Path $resolvedBinDir -Force | Out-Null
        $installedDira = Join-Path $resolvedBinDir 'dira.exe'
        $installedDirad = Join-Path $resolvedBinDir 'dirad.exe'

        if (Test-ExistsOrLink $installedDira) {
            if (Test-DevInstall -Path $installedDira) {
                if (-not $Force) {
                    $devTarget = (@((Get-Item -Path $installedDira -Force).Target)) -join ', '
                    Write-Err "$devTarget is a dev build symlinked at $installedDira -- refusing to overwrite it. Re-run with -Force, or remove the symlink yourself."
                }
                Write-Warn "overwriting dev symlink at $installedDira (-Force)"
            } elseif ((Test-Path -Path $installedDirad) -and
                      ((Get-FileHash -Path $extractedDira -Algorithm SHA256).Hash -eq (Get-FileHash -Path $installedDira -Algorithm SHA256).Hash) -and
                      ((Get-FileHash -Path $extractedDirad -Algorithm SHA256).Hash -eq (Get-FileHash -Path $installedDirad -Algorithm SHA256).Hash)) {
                Write-Info "dira $($release.Version) is already installed at $resolvedBinDir -- nothing to do."
                return
            }
        }

        # 12. was a daemon already running, before we touch anything?
        $daemonWasRunning = $false
        if (Test-Path -Path $installedDira) {
            $daemonWasRunning = (Invoke-BestEffort -Exe $installedDira -Arguments @('daemon', 'status')) -eq 0
        }

        # 13. atomic install.
        Install-Binary -ExtractDir $extractDir -DestDir $resolvedBinDir
        Write-Info "installed dira + dirad $($release.Version) -> $resolvedBinDir"

        # 14. daemon handling: default do nothing. If one was already running, always
        # restart it -- otherwise the user is left with a new CLI nagging about an old
        # daemon. -NoDaemon opts all the way out.
        $elevated = Test-Elevated
        if (-not $NoDaemon -and $elevated) {
            # Deliberately skip auto-start rather than starting a daemon the user
            # will not be able to reach. Installing the binaries is fine elevated;
            # RUNNING the daemon elevated is what breaks capture.
            Write-Warn "this installer is running as Administrator -- NOT starting dirad automatically."
            Write-Warn "a daemon started from here would be elevated, and its control channel would"
            Write-Warn "refuse the ordinary (non-elevated) processes that harness hooks run in --"
            Write-Warn "capture would silently record nothing."
            Write-Info "start it from a NORMAL terminal instead:"
            Write-Info "  $installedDira daemon start"
            Write-Info "  $installedDira daemon install   # logon task, runs unelevated"
        } elseif (-not $NoDaemon) {
            if ($daemonWasRunning) {
                Write-Info "restarting dirad..."
                if ((Invoke-BestEffort -Exe $installedDira -Arguments @('daemon', 'restart')) -ne 0) {
                    Write-Warn "could not restart dirad automatically -- run '$installedDira daemon restart' yourself"
                }
            } elseif ($startDaemon) {
                Write-Info "starting dirad..."
                if ((Invoke-BestEffort -Exe $installedDira -Arguments @('daemon', 'start')) -ne 0) {
                    Write-Warn "could not start dirad automatically -- run '$installedDira daemon start' yourself"
                }
            }
            if ($installService) {
                Write-Info "installing the dirad service..."
                if ((Invoke-BestEffort -Exe $installedDira -Arguments @('daemon', 'install')) -ne 0) {
                    Write-Warn "could not install the dirad service automatically -- run '$installedDira daemon install' yourself"
                }
            } elseif (Test-CanPrompt -NoInteractive:$noInteractive) {
                # Registering a scheduled task is a persistent system change, so it is
                # still never done silently -- but "irm | iex has no usable stdin" was
                # only half true. `iex` consumes the *downloaded script* as text; the
                # console's own input is untouched, which is what Read-Host reads.
                # No console (CI, a redirected pipe, -NoInteractive) keeps the
                # historical hands-off behaviour exactly as it was.
                #
                # Note this arm is unreachable when elevated: the Administrator branch
                # above returns before here, and it must keep precedence. Prompting to
                # install a service from an elevated shell would offer to create
                # exactly the broken setup that branch exists to prevent.
                $answer = Read-Host 'Install dirad as a logon task so it survives reboots? [Y/n]'
                if ([string]::IsNullOrWhiteSpace($answer) -or $answer -match '^(y|yes)$') {
                    Write-Info "installing the dirad service..."
                    if ((Invoke-BestEffort -Exe $installedDira -Arguments @('daemon', 'install')) -ne 0) {
                        Write-Warn "could not install the dirad service automatically -- run '$installedDira daemon install' yourself"
                    }
                } else {
                    Write-Info "skipping the service -- run '$installedDira daemon install' whenever you want it"
                }
            }
        }

        # 15. PATH hint + next steps. One command, because the previous three-line block
        # omitted `dira init` and so left anyone who followed it with a running daemon
        # that captured nothing. It also recommended `daemon start`, which takes the
        # control pipe and blocks the `daemon install` above.
        Add-UserPath -Dir $resolvedBinDir
        Write-Info "`nNext steps:"
        Write-Info "  $installedDira onboard     wire your harnesses, link this device, verify capture"
    } finally {
        Remove-Item -Path $tmp -Recurse -Force -ErrorAction SilentlyContinue
    }
}

Invoke-Main -Version $Version -Channel $Channel -Prerelease:$Prerelease -BinDir $BinDir -Target $Target -Daemon:$Daemon -Service:$Service -NoDaemon:$NoDaemon -NoInteractive:$NoInteractive -Force:$Force -Uninstall:$Uninstall -Help:$Help
# Make the contract stated at the top of this file literally true for every
# caller: a run that reached here did not throw, so it succeeded, and it must
# leave $LASTEXITCODE at 0 -- not merely "at 0 or untouched". `Invoke-BestEffort`
# resets it, but a path that runs no native command at all (a fresh install:
# there is no existing `dira` to probe with `daemon status`) would otherwise
# leave it *unset*, and `$null -ne 0` is TRUE in PowerShell -- so the obvious
# caller-side check, `if ($LASTEXITCODE -ne 0) { throw }`, fires on a completely
# successful install. That is not hypothetical: it broke both windows smoke legs
# of v0.1.1-develop.2. Reached only on success -- a `throw` from Invoke-Main
# skips this line, which is what leaves powershell.exe exiting non-zero for a
# `-File` run. Deliberately the last statement in the file, so truncation can
# only lose the reset, never apply it to a half-finished run.
Set-Variable -Name LASTEXITCODE -Value 0 -Scope Global
# end of install.ps1
