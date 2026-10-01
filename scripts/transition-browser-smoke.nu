#!/usr/bin/env nu

def --wrapped browser [...args: string] {
    let result = ^agent-browser --session $env.PDF_TOOLS_TRANSITION_BROWSER_SESSION ...$args | complete
    if $result.exit_code != 0 { error make {msg: ($result.stdout + $result.stderr)} }
    $result.stdout
}

def assert-browser [condition: string, message: string] {
    browser eval ("if (!(" + $condition + ")) throw new Error(" + ($message | to json --raw) + "); true") | ignore
}

def trace [action: string = ""] {
    browser eval ('(()=>{window.motion=[];window.reversalClicks=[];window.motionEnd=performance.now()+1600;const start=performance.now();function box(selector){const e=document.querySelector(selector);if(!e)return null;const b=e.getBoundingClientRect();return {x:b.x,y:b.y,w:b.width,h:b.height};}function frame(){motion.push({t:performance.now()-start,operation:document.querySelector(".operation-tabs [aria-pressed=true]")?.textContent.trim(),card:box(".ready-card"),header:box(".app-header"),shell:box(".ready-shell"),upload:box(".empty-state"),form:box(".tool-form"),setup:box(".gang-setup"),setupAction:box(".setup-step-actions"),action:box(".tool-form button[type=submit]")});if(performance.now()<motionEnd)requestAnimationFrame(frame)}frame();' + $action + '})()') | ignore
}

def archive [label: string] {
    browser wait --fn 'performance.now()>motionEnd' | ignore
    if $env.PDF_TOOLS_MOTION_FRAMES != "" {
        let sample = browser --json eval ('({label:' + ($label | to json --raw) + ',viewport:{w:innerWidth,h:innerHeight},reduced:matchMedia("(prefers-reduced-motion: reduce)").matches,frames:motion,clicks:window.reversalClicks})') | from json | get data.result
        let previous = open $env.PDF_TOOLS_MOTION_FRAMES
        $previous | append $sample | to json | save --force $env.PDF_TOOLS_MOTION_FRAMES
    }
}

def switch-trace [name: string, label: string] {
    trace ('Array.from(document.querySelectorAll(".operation-tabs button")).find(b=>b.textContent.trim()===' + ($name | to json --raw) + ').click();')
    archive $label
    assert-browser 'motion.length>5 && motion.every(f=>f.card && f.header)' 'Workflow switch lost its shell'
    assert-browser 'motion.every(f=>Math.abs(f.header.x-f.card.x)<1 && Math.abs(f.header.w-f.card.w)<1 && Math.abs((f.card.y-f.header.y-f.header.h)-(motion[0].card.y-motion[0].header.y-motion[0].header.h))<1)' 'Header and card moved independently'
    # On a centered desktop the card grows equally above and below its center.
    # The old structural-class jump moved its top before its height changed.
    assert-browser '(()=>{const a=motion[0].card,b=motion.at(-1).card;if(Math.abs(a.y+a.h/2-b.y-b.h/2)>1)return true;return motion.every(f=>Math.abs(f.card.y+f.card.h/2-b.y-b.h/2)<2)})()' 'Card position jumped ahead of its size animation'
    assert-browser 'motion.filter(f=>f.setup).every(f=>f.setup.y>=f.card.y && f.setup.y+f.setup.h<=f.card.y+f.card.h+1)' 'Imposition setup escaped the animated card'
    assert-browser 'motion.filter(f=>f.setupAction).every(f=>f.setupAction.y>=f.card.y && f.setupAction.y+f.setupAction.h<=f.card.y+f.card.h+1)' 'Imposition progression escaped the animated card'
    assert-browser '(()=>{const gaps=motion.filter(f=>f.action).map(f=>f.card.y+f.card.h-f.action.y-f.action.h);return !gaps.length || Math.max(...gaps)-Math.min(...gaps)<1})()' 'Generic action moved independently of the card'
    assert-browser '(()=>{const f=motion.at(-1);return !f.action || f.action.y+f.action.h<=f.card.y+f.card.h})()' 'Settled generic action escaped its card'
    assert-browser '!matchMedia("(prefers-reduced-motion: reduce)").matches || motion.slice(1).every(f=>["x","y","w","h"].every(k=>Math.abs(f.card[k]-motion.at(-1).card[k])<1))' 'Reduced-motion workflow switch animated geometry'
}

def assert-first-reveal [] {
    browser wait --fn 'performance.now()>motionEnd' | ignore
    assert-browser 'motion.some(f=>f.form) && motion.every(f=>!f.card || f.form)' 'A short intermediate workflow card was displayed'
    assert-browser '(()=>{const frames=motion.filter(f=>f.form);return Math.max(...frames.map(f=>f.card.h))-Math.min(...frames.map(f=>f.card.h))<1;})()' 'The first workflow changed height after being revealed'
}

# Verify loading feedback does not move the selected workspace.
def main [pdf: path, replacement: path, --url: string = "http://127.0.0.1:3200", --session: string = "", --frames: string = ""] {
    $env.PDF_TOOLS_TRANSITION_BROWSER_SESSION = if $session == "" { $"pdf-transition-(random uuid)" } else { $session }
    $env.PDF_TOOLS_MOTION_FRAMES = $frames
    if $frames != "" { [] | to json | save --force $frames }
    let pdf = $pdf | path expand --strict
    let replacement = $replacement | path expand --strict
    let result = try {
        browser close | ignore
        browser open $url | ignore
        browser wait 'main.shell' | ignore
        trace
        browser upload '#file-input' $pdf | ignore
        assert-first-reveal

        let generic_url = browser --json eval 'performance.getEntriesByType("resource").find(r=>r.name.includes("split_generic_tools")).name' | from json | get data.result
        browser open about:blank | ignore
        browser network route $generic_url --abort | ignore
        browser open $url | ignore
        browser upload '#file-input' $pdf | ignore
        browser wait --text 'Retry loading tools' | ignore
        assert-browser '!!document.querySelector(".empty-state") && !document.querySelector(".ready-card")' 'Failed initial loading replaced the upload view'
        trace
        browser network unroute | ignore
        browser find role button click --name 'Retry loading tools' | ignore
        assert-first-reveal
        assert-browser '(()=>{const frames=motion.filter(f=>f.upload);return frames.length>0 && Math.max(...frames.map(f=>f.upload.h))-Math.min(...frames.map(f=>f.upload.h))<1;})()' 'Retry shrank the upload view'
        print 'First upload and failed-module Retry reveal one complete workspace.'

        browser set viewport 1366 900 | ignore
        browser wait --fn 'document.getAnimations().every(a=>a.playState!=="running")' | ignore
        assert-browser '!performance.getEntriesByType("resource").some(r=>r.name.includes("split_imposition"))' 'Cold entry already fetched the imposition module'
        switch-trace 'Impose artwork' 'cold-entry'
        browser wait --text 'Continue to copies' | ignore
        browser eval 'window.imposeResourceCount=performance.getEntriesByType("resource").filter(r=>r.name.includes("split_imposition")).length' | ignore
        assert-browser 'imposeResourceCount>0' 'Cold entry did not fetch the imposition module'
        switch-trace 'PDF to images' 'cold-exit'
        for size in [[1366 900] [1280 633] [800 900]] {
            browser set viewport ...($size | each { into string }) | ignore
            browser wait --fn 'document.getAnimations().every(a=>a.playState!=="running")' | ignore
            switch-trace 'Impose artwork' 'warm-entry'
            assert-browser 'performance.getEntriesByType("resource").filter(r=>r.name.includes("split_imposition")).length===imposeResourceCount' 'Warm entry fetched the imposition module again'
            switch-trace 'PDF to images' 'warm-exit'
            trace 'const buttons=Array.from(document.querySelectorAll(".operation-tabs button"));const impose=buttons.find(b=>b.textContent.trim()==="Impose artwork"),generic=buttons.find(b=>b.textContent.trim()==="PDF to images");impose.click();generic.click();impose.click();generic.click();window.reversalClicks=[];const sequence=[impose,generic,impose,generic];function reverse(){if(performance.now()>motionEnd-100)return;const button=sequence[reversalClicks.length];if(button.disabled){requestAnimationFrame(reverse);return}const running=[".shell",".ready-card",".ready-shell",".app-header"].some(s=>document.querySelector(s).getAnimations().some(a=>a.playState==="running"));button.click();queueMicrotask(()=>{reversalClicks.push({at:performance.now(),name:button.textContent.trim(),selected:button.getAttribute("aria-pressed")==="true",running});if(reversalClicks.length<sequence.length)setTimeout(reverse,35)})}setTimeout(reverse,60);'
            archive 'rapid-reversal'
            assert-browser 'reversalClicks.length===4 && reversalClicks.every(c=>c.selected) && reversalClicks.slice(1).some(c=>c.running)' 'Rapid reversal did not select four enabled operations including an in-flight reversal'
            assert-browser '!!document.querySelector(".tool-form") && !document.querySelector(".gang-setup")' 'Rapid reversal restored a stale workflow'
            assert-browser 'motion.every(f=>f.card && Number.isFinite(f.card.h) && f.card.h>0)' 'Rapid reversal collapsed the card'
            assert-browser 'motion.filter(f=>f.setupAction).every(f=>f.setupAction.y>=f.card.y && f.setupAction.y+f.setupAction.h<=f.card.y+f.card.h+1)' 'Rapid reversal clipped the imposition action'
        }
        print 'Cold/warm entry, exit, and rapid reversal retain coordinated frame geometry.'

        browser set viewport 800 900 | ignore
        browser find role button click --name 'Extract pages' | ignore
        for attempt in [1 2] {
            browser find role button click --name 'Chunks' | ignore
            browser wait --fn 'document.getAnimations().every(a=>a.playState!=="running")' | ignore
            trace
            browser find role button click --name 'Individual' --exact | ignore
            browser wait --fn 'performance.now()>motionEnd' | ignore
            assert-browser '(()=>{const gaps=motion.filter(f=>f.action).map(f=>f.card.y+f.card.h-f.action.y-f.action.h);return gaps.length>0 && Math.max(...gaps)-Math.min(...gaps)<1;})()' 'The action jumped ahead of the shrinking narrow workspace'
        }
        print 'Narrow Chunks collapse keeps the action aligned with the card.'
        browser set viewport 1280 633 | ignore
        browser find role button click --name 'Impose artwork' | ignore
        browser wait --text 'Continue to copies' | ignore
        browser eval 'window.originalFetch=window.fetch;window.fetch=(request,...args)=>request.url?.endsWith("/pdf/inspect") ? new Promise((resolve,reject)=>{window.finishInspection=()=>resolve(originalFetch(request,...args));request.signal.addEventListener("abort",()=>reject(new DOMException("Aborted","AbortError")),{once:true});}) : originalFetch(request,...args);' | ignore
        browser find role button click --name 'Replace artwork' | ignore
        trace
        browser upload '#file-input' $replacement | ignore
        browser wait --text 'Cancel inspection' | ignore
        browser wait --fn 'performance.now()>motionEnd' | ignore
        assert-browser 'motion.every(f=>f.setup) && Math.max(...motion.map(f=>f.setup.y))-Math.min(...motion.map(f=>f.setup.y))<1' 'Inspection moved the existing imposition controls'
        assert-browser 'Math.max(...motion.map(f=>f.setup.h))-Math.min(...motion.map(f=>f.setup.h))<1' 'Inspection resized the existing imposition controls'
        browser find role button click --name 'Cancel inspection' | ignore
        browser wait --fn '!document.querySelector(".inspection-feedback")' | ignore
        browser eval 'window.fetch=window.originalFetch' | ignore
        print 'Artwork inspection preserves workspace geometry and remains cancellable.'

        let impose_url = browser --json eval 'performance.getEntriesByType("resource").find(r=>r.name.includes("split_imposition")).name' | from json | get data.result
        # A new document makes the module cold again without dropping the browser route.
        browser open $url | ignore
        browser upload '#file-input' $pdf | ignore
        browser wait '.tool-form' | ignore
        browser eval 'window.originalFetch=window.fetch;window.fetch=(request,...args)=>String(request?.url??request).includes("split_imposition") ? new Promise(resolve=>{window.finishModule=()=>resolve(originalFetch(request,...args))}) : originalFetch(request,...args)' | ignore
        switch-trace 'Impose artwork' 'delayed-entry'
        assert-browser '!!window.finishModule && !!document.querySelector(".workflow-loading")' 'Delayed module did not exercise loading'
        switch-trace 'PDF to images' 'delayed-exit'
        browser eval 'window.finishModule();window.fetch=window.originalFetch' | ignore
        browser wait 800 | ignore
        assert-browser '!!document.querySelector(".tool-form") && !document.querySelector(".gang-setup")' 'Late module completion restored Impose'
        switch-trace 'Impose artwork' 'delayed-return'
        browser wait '.gang-setup' | ignore

        browser open about:blank | ignore
        browser network route $impose_url --abort | ignore
        browser open $url | ignore
        browser upload '#file-input' $pdf | ignore
        browser wait '.tool-form' | ignore
        switch-trace 'Impose artwork' 'failed-entry'
        browser wait --text 'Retry loading tools' | ignore
        browser network unroute | ignore
        trace 'document.querySelector(".workflow-loading button").click()'
        archive 'impose-retry'
        browser wait '.gang-setup' | ignore
        assert-browser 'Math.max(...motion.map(f=>f.card.h))-Math.min(...motion.map(f=>f.card.h))<1' 'Imposition Retry changed the settled workspace height'

        browser set media light reduced-motion | ignore
        switch-trace 'PDF to images' 'reduced-exit'
        switch-trace 'Impose artwork' 'reduced-entry'
        assert-browser 'matchMedia("(prefers-reduced-motion: reduce)").matches && [".shell",".app-header",".ready-shell",".ready-card"].every(s=>getComputedStyle(document.querySelector(s)).transitionDuration==="0s")' 'Reduced motion retained a workspace transition'
        assert-browser 'motion.slice(1).every(f=>Math.abs(f.card.y-motion.at(-1).card.y)<1 && Math.abs(f.card.h-motion.at(-1).card.h)<1 && Math.abs(f.card.w-motion.at(-1).card.w)<1)' 'Reduced-motion entry animated geometry'
        print 'Delayed completion, failed Impose Retry, and reduced-motion frames pass.'
        null
    } catch {|error| $error }
    if $result != null { print (browser snapshot); print (browser errors) }
    browser close | ignore
    if $result != null { error make $result.raw }
}
