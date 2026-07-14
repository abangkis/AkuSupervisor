param(
    [string]$Config,
    [string]$ServiceId = 'akusidecar'
)

$ErrorActionPreference = 'Stop'

function Resolve-ConfigPath {
    if ($Config) {
        return (Resolve-Path $Config).Path
    }
    if ($env:AKU_SUPERVISOR_CONFIG) {
        return (Resolve-Path $env:AKU_SUPERVISOR_CONFIG).Path
    }
    return (Join-Path $env:LOCALAPPDATA 'AkuSupervisor\services.json')
}

function Invoke-Mcp {
    param([int]$Id, [string]$Method, [object]$Params)

    $message = [ordered]@{
        jsonrpc = '2.0'
        id = $Id
        method = $Method
    }
    if ($null -ne $Params) {
        $message.params = $Params
    }
    $response = Invoke-WebRequest `
        -UseBasicParsing `
        -Method Post `
        -Uri $script:endpoint `
        -Headers $script:headers `
        -ContentType 'application/json' `
        -Body ($message | ConvertTo-Json -Depth 20 -Compress)
    return ($response.Content | ConvertFrom-Json)
}

$configPath = Resolve-ConfigPath
if (-not (Test-Path -LiteralPath $configPath)) {
    throw "AkuSupervisor configuration was not found: $configPath"
}
$settings = Get-Content -LiteralPath $configPath -Raw | ConvertFrom-Json
if (-not $settings.control.mcp.enabled) {
    throw "Read-only MCP is disabled in configuration: $configPath"
}
$tokenPath = Join-Path (Split-Path -Parent $configPath) $settings.control.tokenFile
if (-not (Test-Path -LiteralPath $tokenPath)) {
    throw "Runtime token was not found; start AkuSupervisor first: $tokenPath"
}
$token = (Get-Content -LiteralPath $tokenPath -Raw).Trim()
$script:endpoint = "http://$($settings.control.host):$($settings.control.port)/mcp"
$script:headers = @{
    Authorization = "Bearer $token"
    Accept = 'application/json, text/event-stream'
    'MCP-Protocol-Version' = '2025-11-25'
}

$initialized = Invoke-Mcp 1 'initialize' @{
    protocolVersion = '2025-11-25'
    capabilities = @{}
    clientInfo = @{ name = 'aku-supervisor-test'; version = '1' }
}
if ($initialized.result.protocolVersion -ne '2025-11-25') {
    throw 'MCP initialize returned an unexpected protocol version.'
}

$listed = Invoke-Mcp 2 'tools/list' $null
$expected = @(
    'supervisor_get_recent_events',
    'supervisor_get_service',
    'supervisor_list_services',
    'supervisor_read_logs'
)
$observed = @($listed.result.tools.name | Sort-Object)
if ((Compare-Object $expected $observed).Count -ne 0) {
    throw "Unexpected MCP tool surface: $($observed -join ', ')"
}
if (@($listed.result.tools | Where-Object { -not $_.annotations.readOnlyHint }).Count -ne 0) {
    throw 'Every MCP tool must declare readOnlyHint=true.'
}

$services = Invoke-Mcp 3 'tools/call' @{
    name = 'supervisor_list_services'
    arguments = @{}
}
if ($services.result.isError) {
    throw "Service listing failed: $($services.result.content[0].text)"
}
$service = Invoke-Mcp 4 'tools/call' @{
    name = 'supervisor_get_service'
    arguments = @{ serviceId = $ServiceId }
}
if ($service.result.isError) {
    throw "Service lookup failed: $($service.result.content[0].text)"
}
$events = Invoke-Mcp 5 'tools/call' @{
    name = 'supervisor_get_recent_events'
    arguments = @{ limit = 5 }
}
if ($events.result.isError) {
    throw "Event read failed: $($events.result.content[0].text)"
}
$logs = Invoke-Mcp 6 'tools/call' @{
    name = 'supervisor_read_logs'
    arguments = @{ serviceId = $ServiceId; stream = 'stderr'; lines = 5 }
}
if ($logs.result.isError) {
    throw "Log read failed: $($logs.result.content[0].text)"
}

$originRejected = $false
try {
    Invoke-WebRequest `
        -UseBasicParsing `
        -Method Post `
        -Uri $script:endpoint `
        -Headers ($script:headers + @{ Origin = 'https://attacker.example' }) `
        -ContentType 'application/json' `
        -Body '{"jsonrpc":"2.0","id":7,"method":"tools/list"}' | Out-Null
} catch {
    if ($_.Exception.Response.StatusCode.value__ -eq 403) {
        $originRejected = $true
    } else {
        throw
    }
}
if (-not $originRejected) {
    throw 'An untrusted Origin was not rejected with HTTP 403.'
}

Write-Host "PASS: authenticated read-only MCP at $script:endpoint" -ForegroundColor Green
Write-Host "Configuration: $configPath"
Write-Host "Service: $ServiceId ($($service.result.structuredContent.service.lifecycle) / $($service.result.structuredContent.service.health.status))"
Write-Host "Tools: $($observed -join ', ')"
Write-Host 'Mutation tools: absent; untrusted Origin: rejected.'
