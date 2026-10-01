#!/usr/bin/env nu

const script_dir = path self .

def --wrapped browser [...args: string] {
    # Reveal Artwork-context targets before real browser input.
    if ($args.0 in [click select fill type focus]) {
        browser eval ('(()=>{const e=document.querySelector(' + ($args.1 | to json --raw) + ');if(e?.closest(".gang-setup-fields"))e.scrollIntoView({block:"center",behavior:"instant"});return true})()') | ignore
    } else if ($args.0 == find and ($args | length) > 5 and $args.1 == role and $args.2 == button and $args.3 == click and $args.4 == --name) {
        browser eval ('(()=>{const name=' + ($args.5 | to json --raw) + ';const e=Array.from(document.querySelectorAll(".impose-artwork-controls button, .gang-setup-fields button")).find(e=>(e.getAttribute("aria-label")??e.textContent).trim().includes(name));e?.scrollIntoView({block:"nearest",behavior:"instant"});return true})()') | ignore
    }
    let result = ^agent-browser --session $env.PDF_TOOLS_MIXED_BROWSER_SESSION ...$args | complete
    if $result.exit_code != 0 { error make {msg: (($args | str join ' ') + ": " + $result.stdout + $result.stderr)} }
    $result.stdout
}

def assert-browser [condition: string, message: string] {
    browser eval ("if (!(" + $condition + ")) throw new Error(" + ($message | to json --raw) + "); true") | ignore
}

def export-pdf [count: int] {
    browser eval '(()=>{if(window.exportBusyDelayInstalled)return true;const originalFetch=window.fetch.bind(window);window.fetch=async(...args)=>{const input=args[0],url=typeof input==="string"?input:input.url;if(url.includes("/jobs"))await new Promise(resolve=>setTimeout(resolve,3000));return originalFetch(...args)};window.exportBusyDelayInstalled=true;return true})()' | ignore
    let crop_available = (browser eval '!!document.querySelector(".impose-crop-drag")' | from json)
    if $crop_available {
        browser click '#gang-artwork-tab' | ignore
        browser wait '.impose-crop-drag' | ignore
        browser eval 'window.exportCropBefore=JSON.stringify(latestArtworkLayout()?.request.artworkFit);true' | ignore
        browser click '#gang-setup-tab' | ignore
    }
    browser wait --fn '!!document.querySelector("#gang-download-pdf") && !document.querySelector("#gang-download-pdf").disabled' | ignore
    browser click '#gang-download-pdf' | ignore
    if $crop_available {
        browser click '#gang-artwork-tab' | ignore
        browser wait '.impose-crop-drag' | ignore
        let points = (browser eval '(()=>{const target=document.querySelector(".impose-crop-drag");target.scrollIntoView({block:"center",behavior:"instant"});const r=target.getBoundingClientRect(),x=(r.left+r.right)/2,y=(r.top+r.bottom)/2;return {x:Math.round(x),y:Math.round(y),endX:Math.round(Math.min(r.right-5,x+30))};})()' | from json)
        browser mouse move ($points.x | into string) ($points.y | into string) | ignore
        browser mouse down left | ignore
        browser mouse move ($points.endX | into string) ($points.y | into string) | ignore
        browser mouse up left | ignore
        assert-browser 'JSON.stringify(latestArtworkLayout()?.request.artworkFit)===window.exportCropBefore' 'Crop pointer drag changed artwork positioning during export'
        browser click '#gang-setup-tab' | ignore
    }
    assert-browser '(()=>{const controls=Array.from(document.querySelectorAll(".impose-artwork-mutation-controls select, .impose-artwork-mutation-controls input, .impose-artwork-mutation-controls button, .impose-toolbar-mutation-controls select, .impose-toolbar-mutation-controls input, .impose-toolbar-mutation-controls button")),enabled=controls.filter(control=>!control.matches(":disabled"));if(document.querySelector("#gang-download-pdf")?.disabled&&enabled.length===0)return true;throw new Error("enabled controls: "+enabled.map(control=>control.outerHTML).join(" | "))})()' 'Artwork mutation controls remained writable while export was snapshotting the request'
    browser wait --fn $"artworkDownloads.length>=($count)" | ignore
    assert-browser $"artworkDownloads.length===($count)" $"Expected exactly ($count) artwork downloads"
    browser wait '.export-notice' | ignore
}

def assert-visible-sheet [context: string] {
    browser eval 'assertArtworkToolbarOwner()' | ignore
    browser wait --fn '!!document.querySelector(".sheet-svg .piece-page")' | ignore
    assert-browser '(()=>{const stage=document.querySelector(".sheet-stage").getBoundingClientRect(),svg=document.querySelector(".sheet-svg").getBoundingClientRect();return stage.height>=220 && svg.height>=200 && svg.width>=200 && svg.top>=0 && svg.left>=0 && svg.bottom<=innerHeight+1 && svg.right<=innerWidth+1;})()' ($context + ': the sheet collapsed, was clipped, or left the viewport')
}

def assert-manual-size-required [context: string] {
    browser eval 'assertFinishedSizeControls(true)' | ignore
    assert-browser 'document.querySelector(".setup-continue-button").disabled && (!document.querySelector("#gang-download-pdf") || document.querySelector("#gang-download-pdf").disabled)' ($context + ': source dimensions bypassed manual finished-size acceptance')
}

def reset-crop-position [] {
    # The Artwork panel has its own bounded scrollport.
    browser eval 'assertArtworkContextReachable()' | ignore
    browser find role button click --name 'Reset crop position' --exact | ignore
    browser wait --fn 'latestArtworkLayout()?.request.artworkFit?.position.x===0.5 && latestArtworkLayout()?.request.artworkFit?.position.y===0.5' | ignore
}

def check-artwork-option [label: string] {
    browser eval ('(()=>{const label=Array.from(document.querySelectorAll(".impose-artwork-controls label.checkbox-field")).find(label=>label.textContent.trim()===' + ($label | to json --raw) + ');if(!label)throw new Error("Artwork option missing");label.querySelector("input").scrollIntoView({block:"center",behavior:"instant"});return true})()') | ignore
    browser find label $label check --exact | ignore
}

def assert-pointer-crop-drag [context: string] {
    browser eval 'assertArtworkContextReachable()' | ignore
    reset-crop-position
    browser wait --fn 'latestArtworkLayout()?.request.artworkFit?.position.x===0.5 && !!latestArtworkLayout()?.result.pagePlans[0].positionTravel' | ignore
    # Use the server's signed positionTravel, not full-artwork overflow or guessed fit math.
    # Scroll the real drag surface into its bounded editor, then drive genuine browser mouse input.
    let points = (browser eval '(()=>{const target=document.querySelector(".impose-crop-drag");target.scrollIntoView({block:"center",behavior:"instant"});const r=target.getBoundingClientRect(),panel=document.querySelector(".artwork-toolbar-panel").getBoundingClientRect(),l=latestArtworkLayout(),p=l.result.pagePlans[0],travel=p.positionTravel.x*r.width/p.finishedCutSize.width;if(!Number.isFinite(travel)||Math.abs(travel)<20)throw new Error("Fixture has no useful horizontal crop travel");const x=Math.round((r.left+r.right)/2),y=Math.round((r.top+r.bottom)/2),delta=Math.round(Math.max(-50,Math.min(50,travel*0.1)));if(!delta||x+delta<=r.left||x+delta>=r.right||!document.elementsFromPoint(x,y).some(e=>target.contains(e)||e===target)||!document.elementsFromPoint(x+delta,y).some(e=>target.contains(e)||e===target))throw new Error("Crop drag coordinates are outside the visible unobscured surface");window.cropDragExpectation={position:l.request.artworkFit.position.x+delta/travel,oldX:p.artwork.x,travel:p.positionTravel.x,start:l.request.artworkFit.position.x};return {x,y,endX:x+delta};})()' | from json)
    browser mouse move ($points.x | into string) ($points.y | into string) | ignore
    browser mouse down left | ignore
    browser mouse move ($points.endX | into string) ($points.y | into string) | ignore
    browser mouse up left | ignore
    browser wait --fn 'Math.abs((latestArtworkLayout()?.request.artworkFit?.position.x??-1)-cropDragExpectation.position)<1e-6' | ignore
    assert-browser '(()=>{const p=latestArtworkLayout().result.pagePlans[0],e=cropDragExpectation,img=document.querySelector(".impose-crop-drag image"),box=p.previewBox;return Math.abs(p.artwork.x-e.oldX-e.travel*(e.position-e.start))<1e-6 && img && Math.abs(Number(img.getAttribute("x"))-(p.artwork.x+(box?.left??0)*p.artwork.width/p.sourcePdfSize.width))<1e-6;})()' ($context + ': pointer drag did not follow the authoritative placement plan')
    # Capture the actual rendered sheet placement after the drag, before changing rails.
    browser eval 'window.draggedArtworkState=JSON.stringify(latestArtworkLayout().request.artworkFit);window.draggedSheetPlacement=Array.from(document.querySelectorAll(".sheet-svg .piece-page"),e=>e.outerHTML).join("");true' | ignore
    let narrow = (browser eval 'innerWidth===800' | from json)
    if $narrow {
        browser click '#gang-artwork-tab' | ignore
        browser click '#gang-preview-tab' | ignore
    }
    assert-visible-sheet ($context + ' after pointer drag')
    assert-browser 'JSON.stringify(latestArtworkLayout().request.artworkFit)===draggedArtworkState && Array.from(document.querySelectorAll(".sheet-svg .piece-page"),e=>e.outerHTML).join("")===draggedSheetPlacement' ($context + ': switching to Preview lost the cropped placement')
    if $narrow {
        assert-browser 'document.querySelector("#gang-artwork-panel") && document.querySelector("#gang-preview-tab").getAttribute("aria-selected")==="true" && document.querySelector("#gang-artwork-panel .impose-artwork-controls")' 'Preview rail lost the Artwork context'
        browser click '.sheet-stage .preview-guide-palette > summary' | ignore
        browser wait --fn 'document.querySelector(".sheet-stage .preview-guide-palette")?.open===true' | ignore
        browser eval 'window.previewCutWasChecked=document.querySelector(".sheet-stage .preview-guide-toggles input").checked;true' | ignore
        browser click '.sheet-stage .preview-guide-toggles input' | ignore
        assert-browser 'document.querySelector(".sheet-stage .preview-guide-toggles input").checked!==previewCutWasChecked && (!!document.querySelector(".sheet-svg .piece-cut"))===!previewCutWasChecked && JSON.stringify(latestArtworkLayout().request.artworkFit)===draggedArtworkState' 'Independent Preview guide interaction failed or changed the workspace Artwork state'
        browser click '.sheet-stage .preview-guide-toggles input' | ignore
        assert-browser '(!!document.querySelector(".sheet-svg .piece-cut"))===previewCutWasChecked' 'Preview guide toggle did not restore the cut overlay'
        browser click '#gang-setup-tab' | ignore
        browser eval 'assertArtworkContextReachable()' | ignore
        assert-browser 'Math.abs(Number(document.querySelector("input[aria-label=\"Horizontal crop position\"]").value)-Math.round(cropDragExpectation.position*100))<0.01' 'Returning to the Artwork context lost the dragged crop control value'
    }
    reset-crop-position
}

def assert-manual-bleed-cache [fixture: path, url: string] {
    browser open $url | ignore
    browser set viewport 1366 900 | ignore
    browser wait 'main.shell' | ignore
    browser eval (open --raw ($script_dir | path join mixed-artwork-browser-fixture.js)) | ignore
    # Record the real batch transport and selectively fail both automatic attempts.
    browser eval 'window.bleedPreviewRequests=[];window.bleedPreviewFailures=0;const originalBleedFetch=window.fetch.bind(window);window.fetch=async(input,init)=>{const url=typeof input==="string"?input:input.url;if(/\/gang-up\/sources\/[^/]+\/previews$/.test(url)){const body=JSON.parse(input instanceof Request?await input.clone().text():init?.body);bleedPreviewRequests.push(body);if(bleedPreviewFailures>0){bleedPreviewFailures--;return new Response("temporary preview failure",{status:500});}}return originalBleedFetch(input,init);};window.displayedBleedRaster=()=>document.querySelector(".sheet-svg symbol image")?.getAttribute("href");window.assertBleedRaster=async(color)=>{const href=displayedBleedRaster();if(!href)throw new Error("No displayed source raster");const img=new Image();img.src=href;await img.decode();const c=document.createElement("canvas");c.width=img.naturalWidth;c.height=img.naturalHeight;c.getContext("2d").drawImage(img,0,0);const p=c.getContext("2d").getImageData(2,2,1,1).data;if(color==="red"?!(p[0]>200&&p[2]<60):!(p[2]>200&&p[0]<60))throw new Error("Displayed source raster has stale "+color+" edge: "+Array.from(p));return {href,width:c.width,height:c.height};};true' | ignore
    browser upload '#file-input' ($fixture | path expand --strict) | ignore
    browser wait '.tool-form' | ignore
    browser find role button click --name 'Impose artwork' --exact | ignore
    browser wait 'input[aria-label="Finished width (in)"]' | ignore
    assert-manual-size-required 'Manual bleed PDF'
    browser fill 'input[aria-label="Finished width (in)"]' '8.5' | ignore
    browser fill 'input[aria-label="Finished height (in)"]' '11' | ignore
    browser wait --fn '!!displayedBleedRaster() && latestArtworkLayout()?.request.finishedCutSize.width===8.5' | ignore
    browser eval 'assertBleedRaster("blue").then(v=>{window.defaultBleedRaster=v;return true})' | ignore
    browser find role button click --name 'Continue to copies' --exact | ignore
    browser find role button click --name 'Continue to layout' --exact | ignore
    browser wait --fn '!document.querySelector(".setup-continue-button").disabled' | ignore
    browser find role button click --name 'Continue to bleed' --exact | ignore
    browser click '.bleed-override-button' | ignore
    browser wait --fn 'latestArtworkLayout()?.result.sourceBleedOverride===0.125 && displayedBleedRaster() && displayedBleedRaster()!==defaultBleedRaster.href && bleedPreviewRequests.at(-1)?.sourceBleedOverride===0.125' | ignore
    browser eval 'assertBleedRaster("red").then(v=>{window.manualBleedRaster=v;if(Math.abs(v.width/v.height-4.25/6.25)>0.01||Math.abs(v.width/v.height-defaultBleedRaster.width/defaultBleedRaster.height)<0.005)throw new Error("Manual raster did not expose outside-crop media");return true})' | ignore
    browser find role button click --name 'Clear manual amount' --exact | ignore
    browser wait --fn 'latestArtworkLayout()?.result.sourceBleedOverride==null && displayedBleedRaster() && displayedBleedRaster()!==manualBleedRaster.href && bleedPreviewRequests.at(-1)?.sourceBleedOverride==null' | ignore
    browser eval 'assertBleedRaster("blue").then(v=>{window.clearedBleedRaster=v;return true})' | ignore
    browser eval 'window.bleedPreviewFailures=2;window.bleedRetryRequestStart=bleedPreviewRequests.length;true' | ignore
    browser click '.bleed-override-button' | ignore
    browser wait --text 'Retry artwork' | ignore
    assert-browser 'bleedPreviewFailures===0 && !displayedBleedRaster() && bleedPreviewRequests.slice(bleedRetryRequestStart).every(r=>r.sourceBleedOverride===0.125)' 'Failed manual previews reused the default raster or lost the override during automatic retry'
    browser find role button click --name 'Retry artwork' --exact | ignore
    browser wait --fn '!!displayedBleedRaster() && displayedBleedRaster()!==clearedBleedRaster.href' | ignore
    browser eval 'assertBleedRaster("red").then(()=>true)' | ignore
    assert-browser 'bleedPreviewRequests.at(-1)?.sourceBleedOverride===0.125 && latestArtworkLayout().request.finishedCutSize.width===8.5 && latestArtworkLayout().request.finishedCutSize.height===11' 'Manual retry changed finished size or requested the wrong source raster'
    assert-visible-sheet 'Manual bleed retry'
    print 'Manual source bleed change, clear, automatic failure and explicit retry request/cache/displayed pixels passed.'
}

def assert-fresh-narrow-setup [url: string] {
    browser set viewport 800 900 | ignore
    browser open $url | ignore
    # Opening the already-loaded URL can preserve the current workflow tab and scroll state.
    # Reload before the fresh upload so the bounded Setup geometry is tested from a clean app.
    browser reload | ignore
    browser wait 'main.shell' | ignore
    browser eval (open --raw ($script_dir | path join mixed-artwork-browser-fixture.js)) | ignore
    browser eval 'uploadOddArtwork(900,500,"fresh-narrow-flyer.png")' | ignore
    browser wait '.tool-form' | ignore
    browser find role button click --name 'Impose artwork' --exact | ignore
    browser wait 'input[aria-label="Finished width (in)"]' | ignore
    assert-manual-size-required 'Fresh 800px entry'
    browser fill 'input[aria-label="Finished width (in)"]' '8.5' | ignore
    browser fill 'input[aria-label="Finished height (in)"]' '11' | ignore
    browser select 'select[aria-label="Artwork fitting"]' cover | ignore
    browser eval 'assertArtworkContextReachable()' | ignore
    for action in ['Continue to copies' 'Continue to layout' 'Continue to bleed'] {
        browser click '#gang-setup-tab' | ignore
        browser wait --fn '!document.querySelector(".setup-continue-button").disabled' | ignore
        browser find role button click --name $action --exact | ignore
        browser eval 'assertArtworkContextReachable()' | ignore
    }
    browser eval 'assertArtworkContextReachable()' | ignore
    browser click '#gang-artwork-tab' | ignore
    browser click '#gang-preview-tab' | ignore
    assert-visible-sheet 'Fresh 800px entry independent Preview'
    browser click '#gang-setup-tab' | ignore
    browser eval 'assertArtworkContextReachable()' | ignore
    print 'Fresh 800px entry keeps document/footer bounded across all Setup steps and independent Preview.'
}

# End-to-end: customer image -> requested finished size -> downloaded PDF -> pixels.
def main [mixed_pdf: path, --url: string = "http://127.0.0.1:3200", --bleed-pdf: path] {
    let mixed_pdf = $mixed_pdf | path expand --strict
    $env.PDF_TOOLS_MIXED_BROWSER_SESSION = $"pdf-mixed-(random uuid)"
    let result = try {
        browser open $url | ignore
        browser set viewport 1366 900 | ignore
        browser wait 'main.shell' | ignore
        browser eval (open --raw ($script_dir | path join mixed-artwork-browser-fixture.js)) | ignore
        browser eval 'uploadOddArtwork(900,500,"wide-flyer.png")' | ignore
        browser wait '.tool-form' | ignore
        browser find role button click --name 'Impose artwork' --exact | ignore
        browser wait '.gang-workspace' | ignore
        browser wait 'input[aria-label="Finished width (in)"]' | ignore
        assert-manual-size-required 'Odd-ratio image'
        browser fill 'input[aria-label="Finished width (in)"]' '8.5' | ignore
        browser fill 'input[aria-label="Finished height (in)"]' '11' | ignore
        browser eval 'assertFinishedSizeControls(false)' | ignore
        browser fill 'input[aria-label="Finished width (in)"]' '' | ignore
        assert-browser 'document.querySelector(".setup-continue-button").disabled && document.querySelector("input[aria-label=\"Finished width (in)\"]").getAttribute("aria-invalid")==="true"' 'Clearing an explicit finished width did not block progression'
        browser fill 'input[aria-label="Finished width (in)"]' '8.5' | ignore
        browser wait --fn 'document.querySelector("input[aria-label=\"Finished width (in)\"]").value==="8.5" && document.querySelector("input[aria-label=\"Finished width (in)\"]").getAttribute("aria-invalid")!=="true" && !document.querySelector(".setup-continue-button").disabled' | ignore
        browser eval 'assertFinishedSizeControls(false)' | ignore
        browser select 'select[aria-label="Finished orientation"]' portrait | ignore
        browser select 'select[aria-label="Artwork fitting"]' contain | ignore
        browser eval 'assertArtworkContextReachable()' | ignore
        assert-browser '!document.querySelector("#impose-position-trigger")' 'Position was exposed for Fit'
        browser select 'select[aria-label="Artwork fitting"]' cover | ignore
        browser wait '#impose-position-trigger' | ignore
        browser click '#impose-position-trigger' | ignore
        browser wait --fn 'document.querySelector("#gang-artwork-tab")?.getAttribute("aria-selected")==="true" && document.activeElement?.id==="horizontal-crop-position"' | ignore
        browser find role button click --name 'Done' --exact | ignore
        browser wait --fn 'document.querySelector("#gang-setup-tab")?.getAttribute("aria-selected")==="true" && document.activeElement?.id==="impose-position-trigger"' | ignore
        browser select 'select[aria-label="Artwork fitting"]' contain | ignore
        assert-browser '!document.querySelector("#impose-position-trigger")' 'Position remained exposed for Fit after Done restored Setup'
        browser select 'select[aria-label="Impression orientation"]' upright | ignore
        browser wait --fn 'latestArtworkLayout()?.request.finishedCutSize.width===8.5 && latestArtworkLayout()?.request.finishedCutSize.height===11 && latestArtworkLayout()?.request.orientationPreference==="upright"' | ignore
        assert-browser '(()=>{const l=latestArtworkLayout(),p=l.result.pagePlans[0];return l.request.sourceId && (!l.request.sourcePages || l.request.sourcePages.length===0) && Math.abs(p.artwork.width/p.sourcePdfSize.width-p.artwork.height/p.sourcePdfSize.height)<1e-6;})()' 'Fitting stretched artwork or sent redundant source geometry'
        browser find role button click --name 'Continue to copies' --exact | ignore
        browser eval 'assertArtworkContextReachable()' | ignore
        browser click '#gang-setup-tab' | ignore
        browser wait '#setup-step-job' | ignore
        browser select '.impose-sheet-controls select' '12x18' | ignore
        browser find role button click --name 'Continue to layout' --exact | ignore
        browser eval 'assertArtworkContextReachable()' | ignore
        browser click '#gang-setup-tab' | ignore
        browser wait --fn '!document.querySelector(".setup-continue-button").disabled' | ignore
        browser find role button click --name 'Continue to bleed' --exact | ignore
        browser find role button click --name 'Keep fitted placement' | ignore
        browser eval 'assertArtworkContextReachable()' | ignore
        browser click '#gang-setup-tab' | ignore
        export-pdf 1
        browser eval 'renderDownloadedArtwork(0)' | ignore
        assert-browser 'exportedArtworkRaster.canvas.width===864 && exportedArtworkRaster.canvas.height===1296' 'Exported sheet dimensions changed'
        # Independent expected geometry: Letter centered on 12x18, 900:500 artwork contained.
        browser eval 'assertArtworkPixel(6,4.5,"white");assertArtworkPixel(2.6,9,"red");assertArtworkPixel(6,9,"green");assertArtworkPixel(9.4,9,"blue");true' | ignore

        browser click '#gang-artwork-tab' | ignore
        browser select 'select[aria-label="Artwork fitting"]' cover | ignore
        browser focus 'input[aria-label="Horizontal crop position"]' | ignore
        browser press Home | ignore
        browser wait --fn 'latestArtworkLayout()?.request.artworkFit?.mode==="cover" && latestArtworkLayout()?.request.artworkFit?.position.x===0' | ignore
        browser click '#gang-setup-tab' | ignore
        export-pdf 2
        browser eval 'renderDownloadedArtwork(1).then(()=>{assertArtworkPixel(6,9,"red");return true})' | ignore
        browser find role button click --name 'Scale to add bleed' | ignore
        browser wait --fn 'latestArtworkLayout()?.request.bleedOption==="scaleToBleed" && latestArtworkLayout()?.request.artworkFit?.position.x===0' | ignore
        browser click '#gang-setup-tab' | ignore
        export-pdf 3
        browser eval 'renderDownloadedArtwork(2).then(()=>{assertArtworkPixel(6,9,"red");assertArtworkPixel(1.67,4.5,"red");return true})' | ignore
        browser click '#gang-artwork-tab' | ignore
        browser focus 'input[aria-label="Horizontal crop position"]' | ignore
        browser press End | ignore
        browser wait --fn 'latestArtworkLayout()?.request.artworkFit?.position.x===1' | ignore
        browser click '#gang-setup-tab' | ignore
        export-pdf 4
        browser eval 'renderDownloadedArtwork(3).then(()=>{assertArtworkPixel(6,9,"blue");assertArtworkPixel(10.33,4.5,"blue");return true})' | ignore
        reset-crop-position
        assert-visible-sheet 'Desktop cover editor open'
        assert-pointer-crop-drag 'Desktop cover editor'
        browser set viewport 800 900 | ignore
        browser press Escape | ignore
        browser eval '(()=>{const disclosure=document.querySelector(".artwork-toolbar-disclosure");if(disclosure?.open)disclosure.querySelector(":scope > summary")?.click();return true})()' | ignore
        browser wait --fn '!document.querySelector(".artwork-toolbar-disclosure").open' | ignore
        browser click '#gang-preview-tab' | ignore
        assert-visible-sheet '800x900 cover editor open'
        browser click '#gang-setup-tab' | ignore
        assert-pointer-crop-drag '800x900 cover editor'
        for step in 1..4 {
            browser click $".setup-stepper li:nth-child\(($step)\) button" | ignore
            browser eval 'assertArtworkContextReachable()' | ignore
        }
        browser set viewport 1366 900 | ignore
        print 'Visible desktop/narrow sheets and real pointer crop dragging against positionTravel passed.'
        print 'Odd-ratio finished-size workflow: Fit, Fill, both crop anchors, bleed coverage, reset and actual exported pixels passed.'

        browser select 'select[aria-label="Impression orientation"]' quarterTurn | ignore
        browser wait --fn 'latestArtworkLayout()?.result.rotationDegrees===90' | ignore
        assert-browser '!document.querySelector("#advanced-sheet-settings")' 'Orientation required opening Advanced'
        browser select 'select[aria-label="Impression orientation"]' upright | ignore
        browser find role button click --name 'Keep fitted placement' | ignore
        browser select 'select[aria-label="Artwork fitting"]' stretch | ignore
        browser wait --fn 'latestArtworkLayout()?.request.artworkFit?.mode==="stretch"' | ignore
        assert-browser '!document.querySelector("#impose-position-trigger")' 'Position was exposed for Stretch'
        assert-browser '(()=>{const p=latestArtworkLayout().result.pagePlans[0];return Math.abs(p.artwork.width-8.5)<1e-6 && Math.abs(p.artwork.height-11)<1e-6 && Math.abs(p.artwork.width/p.sourcePdfSize.width-p.artwork.height/p.sourcePdfSize.height)>0.1;})()' 'Stretch did not independently scale both dimensions to the finished cut'
        assert-browser '!document.querySelector(".impose-crop-editor")' 'Stretch incorrectly exposed crop controls'
        browser click '#gang-setup-tab' | ignore
        export-pdf 5
        # Independent expected geometry: all three source stripes reach both ends of Letter.
        # Fit leaves these rows white, while Fill loses at least one outer stripe.
        browser eval 'renderDownloadedArtwork(4).then(()=>{for(const y of [3.7,9,14.3]){assertArtworkPixel(2.0,y,"red");assertArtworkPixel(6,y,"green");assertArtworkPixel(10.0,y,"blue");}assertArtworkPixel(1.4,9,"white");assertArtworkPixel(10.6,9,"white");return true})' | ignore
        print 'Stretch nonproportional, no-crop placement and actual exported edge/center pixels passed.'
        browser upload '#file-input' $mixed_pdf | ignore
        browser wait --fn 'latestArtworkLayout()?.result.pagePlans.length===3 && latestArtworkLayout()?.request.finishedCutSize.width===8.5 && latestArtworkLayout()?.request.finishedCutSize.height===11' | ignore
        # Replacement preserves the current setup step as well as explicit dimensions.
        browser eval 'document.querySelector(".setup-stepper li:first-child button")?.click();true' | ignore
        browser wait '#setup-step-pdf' | ignore
        assert-browser '!!document.querySelector("#setup-step-pdf") && document.querySelector(".setup-stepper li:first-child button").getAttribute("aria-current")==="step"' 'Size navigation did not activate the artwork setup panel'
        browser eval 'assertFinishedSizeControls(false)' | ignore
        assert-browser 'document.querySelector("input[aria-label=\"Finished width (in)\"]").value==="8.5" && document.querySelector("input[aria-label=\"Finished height (in)\"]").value==="11"' 'Replacement PDF discarded explicitly chosen finished dimensions'
        assert-browser '(()=>{const p=latestArtworkLayout().result.pagePlans;return Math.abs(p[0].sourcePdfSize.width-8.5)<1e-6 && Math.abs(p[1].sourcePdfSize.width-8.2639)<0.001 && p[2].sourcePdfSize.width===11;})()' 'Mixed PDF source geometry was flattened to the common cut size'
        browser click '#gang-artwork-tab' | ignore
        browser select 'select[aria-label="Artwork fitting"]' contain | ignore
        browser wait --fn 'latestArtworkLayout()?.result.pagePlans.every(p=>p.finishedCutSize.width===8.5 && p.finishedCutSize.height===11)' | ignore
        browser click '.impose-artwork-advanced > summary' | ignore
        browser wait --fn 'document.querySelector(".impose-artwork-advanced")?.open===true' | ignore
        browser select 'select[aria-label="Selected artwork"]' '2' | ignore
        check-artwork-option 'Adjust only selected artwork'
        check-artwork-option "Override this artwork's finished size"
        browser fill 'input[aria-label="Artwork finished width (in)"]' '6' | ignore
        browser fill 'input[aria-label="Artwork finished height (in)"]' '9' | ignore
        browser wait --fn 'latestArtworkLayout()?.result.pagePlans[1].finishedCutSize.width===6 && latestArtworkLayout()?.result.pagePlans[1].finishedCutSize.height===9' | ignore
        assert-browser 'latestArtworkLayout().result.pagePlans[0].finishedCutSize.width===8.5 && latestArtworkLayout().result.pagePlans[2].finishedCutSize.width===8.5' 'A selected-piece override resized unrelated artwork'
        browser click '#gang-setup-tab' | ignore
        browser find role button click --name 'Continue to copies' --exact | ignore
        assert-browser 'document.querySelector(".printing-control option[value=double]").disabled' 'Odd artwork count enabled unpaired duplex'
        browser find role button click --name 'Continue to layout' --exact | ignore
        browser wait --fn '!document.querySelector(".setup-continue-button").disabled' | ignore
        browser find role button click --name 'Continue to bleed' --exact | ignore
        export-pdf 6
        browser eval '(async()=>{const form=new FormData();form.append("file",artworkDownloads[5].blob,"mixed-imposed.pdf");const response=await fetch("/pdf/inspect",{method:"POST",body:form});if(!response.ok||(await response.json()).pageCount!==latestArtworkLayout().result.sheetsRequired*(latestArtworkLayout().result.duplex?2:1))throw new Error("Mixed output sheet count disagrees with the placement plan");return true})()' | ignore
        browser set viewport 800 900 | ignore
        browser eval '(()=>{const disclosure=document.querySelector(".artwork-toolbar-disclosure");if(disclosure?.open)disclosure.querySelector(":scope > summary")?.click();return true})()' | ignore
        browser click '#gang-preview-tab' | ignore
        assert-visible-sheet '800x900 mixed artwork'
        assert-browser 'document.documentElement.scrollWidth<=innerWidth' 'Mixed-artwork controls overflow half-screen width'
        browser click '#gang-setup-tab' | ignore
        browser eval 'assertArtworkContextReachable();document.querySelector("select[aria-label=\"Impression orientation\"]").scrollIntoView({block:"center",behavior:"instant"});true' | ignore
        assert-browser '(()=>{const r=document.querySelector("select[aria-label=\"Impression orientation\"]").getBoundingClientRect();return r.width>0 && r.left>=0 && r.right<=innerWidth;})()' 'Orientation is not reachable at half-screen width'
        print 'Mixed PDF manual acceptance, common sizing, per-piece override, odd duplex gating, export and half-screen controls passed.'
        if $bleed_pdf != null { assert-manual-bleed-cache $bleed_pdf $url }
        assert-fresh-narrow-setup $url
        null
    } catch {|error| $error }
    if $result != null {
        print (browser snapshot)
        print (browser errors)
        let evidence = $env.JCODE_SCRATCH_DIR? | default $script_dir | path join $"($env.PDF_TOOLS_MIXED_BROWSER_SESSION)-failure.png"
        try { browser screenshot $evidence | print }
    }
    browser close | ignore
    if $result != null { error make $result.raw }
}
