#!/usr/bin/env nu

const project_dir = path self ..

def --wrapped checked [...args: string] {
    let result = ^docker ...$args | complete
    if $result.exit_code != 0 { error make {msg: ($result.stdout + $result.stderr)} }
    $result.stdout | str trim
}

def --wrapped browser [...args: string] {
    let result = ^agent-browser --session $env.PDF_TOOLS_DEPLOYMENT_BROWSER_SESSION ...$args | complete
    if $result.exit_code != 0 { error make {msg: ($result.stdout + $result.stderr)} }
    $result.stdout
}

def assert-browser [condition: string, message: string] {
    browser eval ("if (!(" + $condition + ")) throw new Error(" + ($message | to json --raw) + "); true") | ignore
}

# Disposable container only. Never touches Compose services or persistent shop data.
def main [--image: string = "pdf-tools-pr7:validation", --skip-build] {
    if not $skip_build {
        ^docker build --progress=plain --tag $image $project_dir
        if $env.LAST_EXIT_CODE != 0 { error make {msg: "Container build failed"} }
    }
    let name = $"pdf-tools-validation-(random uuid)"
    let selected_port = port 31000 51000
    let url = $"http://127.0.0.1:($selected_port)"
    let scratch = $env.JCODE_SCRATCH_DIR? | default ($project_dir | path join target)
    let fixtures = $scratch | path join $name
    ^nu --no-config-file ($project_dir | path join scripts smoke-fixtures.nu) $fixtures
    if $env.LAST_EXIT_CODE != 0 { error make {msg: "Could not create container smoke fixtures"} }
    $env.PDF_TOOLS_DEPLOYMENT_BROWSER_SESSION = $name
    let result = try {
        checked run --detach --rm --name $name --publish $"127.0.0.1:($selected_port):3000" $image | ignore
        mut ready = false
        for _ in 1..100 {
            if (try { http get --raw --max-time 1sec $"($url)/health" } catch { "" }) == "ok" { $ready = true; break }
            sleep 100ms
        }
        if not $ready { error make {msg: "Container did not become healthy"} }
        let index = http get --raw $url
        let assets = $index | parse --regex '/(?P<name>pkg/[a-f0-9]{64}/pdf-tools-frontend\.[A-Za-z0-9_-]+\.(?:js|wasm|css))' | get name | uniq
        if ($assets | length) != 3 { error make {msg: "Container does not serve a versioned production bundle"} }
        for asset in $assets {
            let headers = $fixtures | path join headers
            let bytes = $fixtures | path join asset.gz
            ^curl --fail --silent --show-error --max-time 10 --header "Accept-Encoding: gzip" --dump-header $headers --output $bytes $"($url)/($asset)"
            if $env.LAST_EXIT_CODE != 0 { error make {msg: "Container asset request failed"} }
            if not (open --raw $headers | str downcase | str contains "content-encoding: gzip") { error make {msg: "Container asset was not compressed"} }
            ^gzip --test $bytes
            if $env.LAST_EXIT_CODE != 0 { error make {msg: "Container served invalid gzip"} }
        }
        ^nu --no-config-file ($project_dir | path join scripts workflow-browser-smoke.nu) --url $url
        if $env.LAST_EXIT_CODE != 0 { error make {msg: "Container workflow smoke failed"} }
        ^nu --no-config-file ($project_dir | path join scripts mixed-artwork-browser-smoke.nu) ($fixtures | path join mixed.pdf) --bleed-pdf ($fixtures | path join manual-bleed.pdf) --url $url
        if $env.LAST_EXIT_CODE != 0 { error make {msg: "Container mixed-artwork smoke failed"} }

        # Exercise an actual missing deployed module, not a mocked fetch failure.
        browser open $url | ignore
        browser wait 'main.shell' | ignore
        browser upload '#file-input' ($fixtures | path join source.pdf) | ignore
        browser wait '.tool-form' | ignore
        let loader_url = browser --json eval 'performance.getEntriesByType("resource").find(x=>x.name.includes("/__wasm_split.")).name' | from json | get data.result
        let module = http get --raw $loader_url | parse --regex '\./(?P<name>split_imposition[^" ]+\.wasm)' | get name | first
        let package = $assets.0 | path dirname
        let deployed = $"/app/frontend/dist/($package)/($module)"
        checked exec $name mv $deployed $"($deployed).unavailable" | ignore
        checked exec $name mv $"($deployed).gz" $"($deployed).gz.unavailable" | ignore
        browser find role button click --name 'Impose artwork' | ignore
        browser wait --text 'Retry loading tools' | ignore
        assert-browser '!!document.querySelector(".workflow-recovery-hint a")' 'An old tab has no reload recovery'
        assert-browser 'document.querySelector(".workflow-recovery-hint").textContent.includes("clears your selected files and settings")' 'Reload recovery does not explain state loss'
        browser find role button click --name 'Retry loading tools' | ignore
        browser wait --text 'Retry loading tools' | ignore
        assert-browser 'document.querySelector(".app-file-context strong").textContent==="source.pdf"' 'Retry discarded the old tab selection'
        checked exec $name mv $"($deployed).unavailable" $deployed | ignore
        checked exec $name mv $"($deployed).gz.unavailable" $"($deployed).gz" | ignore
        browser click '.workflow-recovery-hint a' | ignore
        browser wait '.empty-state' | ignore
        browser upload '#file-input' ($fixtures | path join source.pdf) | ignore
        browser wait '.tool-form' | ignore
        browser find role button click --name 'Impose artwork' | ignore
        browser wait '.gang-workspace' | ignore
        print 'Container health, versioned gzip assets, five exports, and missing deployed-module Retry/reload recovery passed.'
        null
    } catch {|error| $error }
    if $result != null { try { print (checked logs $name) } }
    try { browser close | ignore }
    try { checked stop $name | ignore }
    # Keep small fixtures and request headers for inspection if the smoke failed.
    if $result == null { rm --recursive $fixtures }
    if $result != null { error make $result.raw }
}
