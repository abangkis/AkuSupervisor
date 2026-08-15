Set-StrictMode -Version Latest

function Get-PartialStartupServiceIds {
    param(
        [Parameter(Mandatory)] [object] $Configuration,
        [Parameter(Mandatory)] [string[]] $RequestedServiceIds
    )

    $properties = @($Configuration.services.PSObject.Properties)
    $propertiesById = @{}
    foreach ($property in $properties) {
        $propertiesById[[string] $property.Name] = $property
    }

    $included = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    $pending = [Collections.Generic.Stack[string]]::new()
    for ($index = $RequestedServiceIds.Count - 1; $index -ge 0; $index--) {
        $pending.Push([string] $RequestedServiceIds[$index])
    }

    while ($pending.Count -gt 0) {
        $serviceId = $pending.Pop()
        if (-not $included.Add($serviceId)) {
            continue
        }
        $property = $propertiesById[$serviceId]
        if ($null -eq $property) {
            throw "Cannot create partial startup configuration: service '$serviceId' is not registered."
        }
        $dependencyProperty = $property.Value.PSObject.Properties['dependsOn']
        [object[]] $dependencies = @()
        if ($null -ne $dependencyProperty) {
            $dependencies = @($dependencyProperty.Value)
        }
        for ($index = $dependencies.Count - 1; $index -ge 0; $index--) {
            $dependency = [string] $dependencies[$index]
            if (-not $propertiesById.ContainsKey($dependency)) {
                throw "Cannot create partial startup configuration: service '$serviceId' depends on unregistered service '$dependency'."
            }
            $pending.Push($dependency)
        }
    }

    # Preserve source order for a stable, reviewable generated profile. The
    # Supervisor remains responsible for the actual topological startup plan.
    return @($properties |
        Where-Object { $included.Contains([string] $_.Name) } |
        ForEach-Object { [string] $_.Name })
}

function New-PartialStartupConfiguration {
    param(
        [Parameter(Mandatory)] [object] $Configuration,
        [Parameter(Mandatory)] [string[]] $ServiceIds,
        [Parameter(Mandatory)] [string] $SourcePath,
        [Parameter(Mandatory)] [string] $WatcherId
    )

    if ($ServiceIds.Count -eq 0) {
        throw '-PartialStart requires at least one startup service ID.'
    }

    $sourceFullPath = [IO.Path]::GetFullPath($SourcePath)
    $sourceDirectory = Split-Path -Parent $sourceFullPath
    $partialPath = Join-Path $sourceDirectory ".aku-supervisor.partial-$WatcherId.json"
    $partialConfiguration = ($Configuration | ConvertTo-Json -Depth 100) | ConvertFrom-Json
    $selectedServices = [ordered]@{}
    foreach ($serviceId in $ServiceIds) {
        $property = $partialConfiguration.services.PSObject.Properties[$serviceId]
        if ($null -eq $property) {
            throw "Cannot create partial startup configuration: service '$serviceId' is not registered."
        }
        $selectedServices[$serviceId] = $property.Value
    }
    $partialConfiguration.services = [pscustomobject] $selectedServices
    [IO.File]::WriteAllText(
        $partialPath,
        (($partialConfiguration | ConvertTo-Json -Depth 100) + "`n"),
        [Text.UTF8Encoding]::new($false))
    return $partialPath
}

function Remove-StalePartialStartupConfigurations {
    param(
        [Parameter(Mandatory)] [string] $SourcePath,
        [Parameter(Mandatory)] [string] $CurrentWatcherId
    )

    $sourceDirectory = Split-Path -Parent ([IO.Path]::GetFullPath($SourcePath))
    $currentName = ".aku-supervisor.partial-$CurrentWatcherId.json"
    foreach ($candidate in [IO.Directory]::EnumerateFiles(
            $sourceDirectory,
            '.aku-supervisor.partial-*.json',
            [IO.SearchOption]::TopDirectoryOnly)) {
        if ([IO.Path]::GetFileName($candidate) -eq $currentName) {
            continue
        }
        [IO.File]::Delete($candidate)
        Write-Host "[watch] Removed stale partial startup configuration: $candidate" -ForegroundColor DarkGray
    }
}
