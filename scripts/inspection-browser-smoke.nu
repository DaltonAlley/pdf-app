#!/usr/bin/env nu

def --wrapped browser [...args: string] {
    let result = ^agent-browser --session $env.PDF_TOOLS_INSPECTION_BROWSER_SESSION ...$args | complete
    if $result.exit_code != 0 { error make {msg: ($result.stdout + $result.stderr)} }
    $result.stdout
}

def assert-browser [condition: string] {
    browser eval ("if (!(" + $condition + ")) throw new Error('Inspection regression'); true") | ignore
}

# Exercise inspection cancellation without depending on backend response timing.
def main [pdf: path, replacement: path, corrupt: path, --url: string = "http://127.0.0.1:3200"] {
    $env.PDF_TOOLS_INSPECTION_BROWSER_SESSION = $"pdf-inspection-(random uuid)"
    let pdf = $pdf | path expand --strict
    let replacement = $replacement | path expand --strict
    let corrupt = $corrupt | path expand --strict
    let result = try {
        browser open $url | ignore
        for mode in [empty generic impose late] {
            browser reload | ignore
            browser wait 'main.shell' | ignore
            if $mode != empty {
                browser upload '#file-input' $pdf | ignore
                browser wait '.tool-form' | ignore
                if $mode == impose {
                    browser find role button click --name 'Impose artwork' | ignore
                    browser wait --text 'Continue to copies' | ignore
                }
            }
            browser eval 'window.inspectionRequests=0; window.originalFetch=window.fetch; window.ignoreAbort=false; window.fetch=(request,...args)=>request.url?.endsWith("/pdf/inspect") ? new Promise((resolve,reject)=>{window.inspectionRequests++; window.releaseInspection=()=>resolve(new Response(JSON.stringify({pageCount:5}),{status:200})); request.signal.addEventListener("abort",()=>{if(!window.ignoreAbort) reject(new DOMException("Aborted","AbortError"));},{once:true});}) : window.originalFetch(request,...args);' | ignore
            if $mode == late { browser eval 'window.ignoreAbort=true' | ignore }
            browser upload '#file-input' $replacement $replacement | ignore
            browser wait --text 'Checking PDFs' | ignore
            let cancel_name = if $mode in [empty impose] { 'Cancel inspection' } else { 'Cancel' }
            browser find role button click --name $cancel_name --exact | ignore
            if $mode == late { browser eval 'window.releaseInspection()' | ignore }
            browser wait --fn '!document.querySelector("#app-startup") && ![...document.querySelectorAll("button")].some(b=>b.textContent.trim()==="Cancel inspection" || b.textContent.trim()==="Cancel")' | ignore
            assert-browser 'window.inspectionRequests===1'
            if $mode == empty {
                assert-browser '!!document.querySelector(".empty-state") && !document.querySelector(".tool-form")'
            } else {
                let expected = $pdf | path basename | to json --raw
                assert-browser ('document.querySelector(".app-file-context strong").textContent===' + $expected)
                if $mode == impose { assert-browser '!!document.querySelector(".gang-workspace")' }
            }
            browser eval 'window.fetch=window.originalFetch' | ignore
            browser upload '#file-input' $replacement | ignore
            let replacement_name = $replacement | path basename | to json --raw
            browser wait --fn ('document.querySelector(".app-file-context strong")?.textContent===' + $replacement_name) | ignore
            if $mode == impose {
                browser wait --text 'Continue to copies' | ignore
                browser upload '#file-input' $corrupt | ignore
                browser wait --text 'damaged or is not a readable PDF' | ignore
                assert-browser ('document.querySelector(".app-file-context strong").textContent===' + $replacement_name)
                assert-browser '!!document.querySelector(".gang-workspace") && !!document.querySelector("[role=alert]")'
                assert-browser '(()=>{const error=document.querySelector(".ready-card [role=alert]").getBoundingClientRect(); const workspace=document.querySelector(".gang-workspace").getBoundingClientRect(); return error.height>0 && error.top>=0 && error.bottom<=workspace.top && workspace.bottom<=innerHeight;})()'
            }
            print $"Inspection cancellation, retained state, and retry passed: ($mode)."
        }
        null
    } catch {|error| $error }
    if $result != null { print (browser snapshot); print (browser errors) }
    browser close | ignore
    if $result != null { error make $result.raw }
}
