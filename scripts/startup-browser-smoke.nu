#!/usr/bin/env nu

def --wrapped browser [...args: string] {
    let result = ^agent-browser --session $env.PDF_TOOLS_STARTUP_BROWSER_SESSION ...$args | complete
    if $result.exit_code != 0 {
        error make { msg: (($args | str join ' ') + ": " + $result.stdout + $result.stderr) }
    }
    $result.stdout
}

def assert-browser [condition: string, message: string] {
    let script = "if (!(" + $condition + ")) throw new Error(" + ($message | to json --raw) + "); true"
    browser eval $script | ignore
}

# Exercise deferred loading against a running app using a small valid PDF.
def main [pdf: path, --url: string = "http://127.0.0.1:3200"] {
    $env.PDF_TOOLS_STARTUP_BROWSER_SESSION = $"pdf-startup-(random uuid)"
    let pdf = $pdf | path expand --strict
    let result = try {
        browser close | ignore
        browser open $url | ignore
        browser wait "main.shell" | ignore
        assert-browser '!document.getElementById("app-startup")' "startup overlay remained"
        assert-browser '!performance.getEntriesByType("resource").some(x => /\/(split_|chunk_).*\.wasm/.test(x.name))' "workflow code loaded before file selection"

        let loader = browser --json eval 'performance.getEntriesByType("resource").find(x => x.name.includes("/__wasm_split.")).name'
            | from json | get data.result
        let package_url = $loader | str replace --regex '/[^/]+$' ''
        let modules = http get --raw $loader
            | parse --regex '\./(?P<name>split_[^"]+\.wasm)'
            | get name
        let generic_module = $modules | where {|name| $name | str starts-with split_generic_tools } | first
        let impose_module = $modules | where {|name| $name | str starts-with split_imposition } | first
        let entry_assets = http get --raw $url
            | parse --regex '/(?P<name>pkg/(?:[a-f0-9]{64}/)?pdf-tools-frontend[^"]*\.(?:css|js|wasm))'
            | get name

        browser network route $"($package_url)/($generic_module)" --abort | ignore
        browser upload "#file-input" $pdf | ignore
        browser wait --text "Retry loading tools" | ignore
        browser eval 'window.selectedFile = document.getElementById("file-input").files[0]; window.apiCount = performance.getEntriesByType("resource").filter(x => /\/(jobs|gang-up|pdf\/inspect)/.test(x.name)).length;' | ignore
        browser network unroute | ignore
        browser find role button click --name "Retry loading tools" | ignore
        browser wait ".tool-form" | ignore
        assert-browser 'document.getElementById("file-input").files[0] === window.selectedFile' "retry replaced the selected file"
        assert-browser 'performance.getEntriesByType("resource").filter(x => /\/(jobs|gang-up|pdf\/inspect)/.test(x.name)).length === window.apiCount' "generic retry repeated PDF preflight"
        browser find role button click --name "JPEG" | ignore

        browser network route $"($package_url)/($impose_module)" --abort | ignore
        browser find role button click --name "Impose artwork" | ignore
        browser wait --text "Retry loading tools" | ignore
        assert-browser 'document.getElementById("file-input").files[0] === window.selectedFile' "failed imposition load lost the selected file"
        browser network unroute | ignore
        browser find role button click --name "Retry loading tools" | ignore
        browser wait ".gang-workspace" | ignore
        browser wait --fn '[...document.querySelectorAll("button")].some(x => x.textContent.trim() === "PDF to images" && !x.disabled)' | ignore
        browser find role button click --name "PDF to images" | ignore
        browser wait ".tool-form" | ignore
        assert-browser '[...document.querySelectorAll("button")].some(x => x.textContent.trim() === "JPEG" && x.getAttribute("aria-pressed") === "true")' "workflow loading reset PDF settings"
        print "Deferred workflow requests, retry, file retention, and settings passed."

        browser close | ignore
        browser open $url | ignore
        browser wait 'main.shell' | ignore
        browser upload '#file-input' $pdf | ignore
        browser wait '.tool-form' | ignore
        browser eval 'window.originalFetch=window.fetch;window.impositionReturned=false;window.impositionInstantiated=false;window.originalInstantiate=WebAssembly.instantiateStreaming.bind(WebAssembly);WebAssembly.instantiateStreaming=async(...args)=>{const result=await originalInstantiate(...args);window.impositionInstantiated=true;return result;};window.fetch=(request,...args)=>String(request?.url??request).includes("split_imposition")?new Promise(resolve=>{window.releaseImposition=()=>originalFetch(request,...args).then(response=>{window.impositionReturned=true;resolve(response)});}):originalFetch(request,...args);' | ignore
        browser find role button click --name 'Impose artwork' | ignore
        browser wait --fn 'typeof window.releaseImposition==="function"' | ignore
        browser find role button click --name 'PDF to images' | ignore
        browser wait '.tool-form' | ignore
        browser eval 'window.releaseImposition();true' | ignore
        browser wait --fn 'window.impositionReturned && window.impositionInstantiated' | ignore
        assert-browser '!!document.querySelector(".tool-form") && !document.querySelector(".gang-workspace")' 'A late module response restored the workflow the user left'
        browser eval 'window.fetch=window.originalFetch' | ignore
        print 'Leaving a deferred workflow before its response preserves the active workflow.'

        for extension in [css js wasm] {
            browser close | ignore
            browser open about:blank | ignore
            let asset = $entry_assets | where {|name| $name | str ends-with $".($extension)" } | first
            browser network route $"($url)/($asset)" --abort | ignore
            browser open $url | ignore
            browser wait "#app-startup-retry:not([hidden])" | ignore
            assert-browser 'document.getElementById("app-startup-status").textContent.includes("couldn’t start")' "startup did not report the failed asset"
            browser network unroute | ignore
            browser click "#app-startup-retry" | ignore
            browser wait "main.shell" | ignore
            assert-browser '!document.getElementById("app-startup")' "startup retry failed"
        }
        print "CSS, JavaScript and Wasm startup recovery passed."
        null
    } catch {|error| $error }
    if $result != null {
        print (browser snapshot)
        print (browser errors)
    }
    browser close | ignore
    if $result != null { error make $result.raw }
}
