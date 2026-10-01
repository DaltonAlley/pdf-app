#!/usr/bin/env nu

def --wrapped browser [...args: string] {
    # Reveal Artwork-context targets before real browser input.
    if ($args.0 in [click select fill type focus]) {
        browser eval ('(()=>{const e=document.querySelector(' + ($args.1 | to json --raw) + ');if(e?.closest(".gang-setup-fields"))e.scrollIntoView({block:"center",behavior:"instant"});return true})()') | ignore
    } else if ($args.0 == find and ($args | length) > 5 and $args.1 == role and $args.2 == button and $args.3 == click and $args.4 == --name) {
        browser eval ('(()=>{const name=' + ($args.5 | to json --raw) + ';const e=Array.from(document.querySelectorAll(".impose-artwork-controls button, .gang-setup-fields button")).find(e=>(e.getAttribute("aria-label")??e.textContent).trim().includes(name));e?.scrollIntoView({block:"nearest",behavior:"instant"});return true})()') | ignore
    }
    let result = ^agent-browser --session $env.PDF_TOOLS_WORKFLOW_BROWSER_SESSION ...$args | complete
    if $result.exit_code != 0 { error make {msg: (($args | str join ' ') + ": " + $result.stdout + $result.stderr)} }
    $result.stdout
}

def assert-browser [condition: string, message: string] {
    browser eval ("if (!(" + $condition + ")) throw new Error(" + ($message | to json --raw) + "); true") | ignore
}

def assert-artwork-context [] {
    assert-browser '(()=>{const labels=Array.from(document.querySelectorAll(".setup-stepper .setup-step-label"),e=>e.textContent.trim());return JSON.stringify(labels)===JSON.stringify(["Size","Quantity & sheet","Arrangement","Bleed"]) && document.querySelectorAll(".setup-stepper").length===1 && document.querySelectorAll("#gang-setup-tab,#gang-artwork-tab,#gang-preview-tab").length===3 && document.querySelectorAll(".setup-step-status").length===0;})()' 'The Setup rail must contain exactly the four labels without numeric indicators'
    assert-browser '(()=>{const e=document.querySelector(".impose-artwork-controls"),selects=Array.from(e?.querySelectorAll("select")??[]),toolbar=document.querySelector(".impose-workspace-toolbar"),controls=document.querySelector(".impose-workspace-toolbar-controls"),presets=document.querySelector(".preset-toolbar"),options=selects.find(select=>select.getAttribute("aria-label")==="Artwork fitting")?.options;return document.querySelectorAll(".impose-artwork-controls").length===1 && !!document.querySelector("#gang-artwork-panel") && !document.querySelector(".gang-setup-fields .impose-artwork-controls") && !!document.querySelector("#gang-artwork-panel .impose-artwork-controls") && e?.querySelector("h3")?.textContent.trim()==="Artwork" && !!e?.querySelector(".impose-artwork-head") && !!e?.querySelector(".impose-artwork-advanced") && !!document.querySelector("#gang-artwork-tab") && presets?.closest(".impose-toolbar-mutation-controls")?.parentElement===controls && !document.querySelector(".gang-setup-fields .presets") && JSON.stringify(Array.from(options??[],option=>option.textContent))===JSON.stringify(["Fit","Stretch","Fill"]) && selects.some(select=>select.getAttribute("aria-label")==="Impression orientation");})()' 'The Artwork context, Presets placement, or Artwork option order is incorrect'
    browser click '#gang-artwork-tab' | ignore
}

def upload [name: string, sizes: list<list<int>>] {
    browser eval ("polishUpload(" + ($name | to json --raw) + "," + ($sizes | to json --raw) + ")") | ignore
    browser wait --fn ('document.querySelector(".app-file-context strong")?.textContent===' + ($name | to json --raw)) | ignore
}

# Real PDFs and jobs, with controlled failures at the browser transport boundary.
def main [--url: string = "http://127.0.0.1:3200"] {
    $env.PDF_TOOLS_WORKFLOW_BROWSER_SESSION = $"pdf-workflow-(random uuid)"
    let result = try {
        browser open $url | ignore
        browser set viewport 1366 900 | ignore
        browser wait 'main.shell' | ignore
        browser eval 'window.assertArtworkToolbarOwner=()=>{const c=document.querySelectorAll(".impose-artwork-controls");if(c.length!==1||!c[0].closest(".impose-workspace-toolbar")||c[0].closest(".gang-setup-fields")||!c[0].closest(".artwork-toolbar-panel")||document.querySelector("#gang-preview-panel .impose-artwork-controls, #gang-preview-panel .impose-crop-editor"))throw new Error("The Artwork context must exist exactly once in the workspace toolbar and be absent from Setup and Preview");return true};window.assertArtworkContextReachable=async(selector="select[aria-label=\"Artwork fitting\"]")=>{assertArtworkToolbarOwner();const d=document.querySelector(".artwork-toolbar-disclosure"),summary=d?.querySelector(":scope > summary"),panel=document.querySelector(".artwork-toolbar-panel"),toolbar=document.querySelector(".impose-workspace-toolbar"),stepper=document.querySelector(".setup-stepper"),actions=document.querySelector(".setup-step-actions"),stage=document.querySelector(".sheet-stage");if(!d||!summary||!panel||!toolbar||!stepper||!actions||!stage)throw new Error("The Artwork toolbar, sheet stage, Setup stepper, or sticky actions are missing");const before={toolbar:toolbar.getBoundingClientRect().toJSON(),stepper:stepper.getBoundingClientRect().toJSON(),actions:actions.getBoundingClientRect().toJSON(),stage:stage.getBoundingClientRect().toJSON()};if(!d.open)summary.click();await new Promise(resolve=>requestAnimationFrame(resolve));const target=panel.querySelector(selector);if(!target)throw new Error("Artwork toolbar control is missing: "+selector);target.scrollIntoView({block:"nearest",behavior:"instant"});const r=target.getBoundingClientRect(),p=panel.getBoundingClientRect(),afterActions=actions.getBoundingClientRect(),afterStepper=stepper.getBoundingClientRect(),afterStage=stage.getBoundingClientRect(),checks={horizontalOverflow:document.documentElement.scrollWidth>innerWidth+1,panelAboveViewport:p.top< -1,panelBelowViewport:p.bottom>innerHeight+1,targetAbovePanel:r.top<p.top,targetBelowPanel:r.bottom>p.bottom,targetNotHitTestable:!target.contains(document.elementFromPoint(r.x+r.width/2,r.y+r.height/2)),toolbarMoved:Math.abs(before.toolbar.top-toolbar.getBoundingClientRect().top)>1,stepperMoved:Math.abs(before.stepper.top-afterStepper.top)>1,footerMoved:Math.abs(before.actions.top-afterActions.top)>1,stageMoved:Math.abs(before.stage.top-afterStage.top)>1||Math.abs(before.stage.height-afterStage.height)>1,footerBelowViewport:afterActions.bottom>innerHeight+1};if(Object.values(checks).some(Boolean))throw new Error("The Artwork toolbar is clipped, moves the workspace, or scrolls the sticky actions away: "+JSON.stringify({selector,checks,target:r.toJSON(),panel:p.toJSON(),before,afterActions:afterActions.toJSON(),viewport:innerHeight}));return true};true' | ignore
        browser eval 'window.polishUpload=(name,sizes)=>{const objects=["<< /Type /Catalog /Pages 2 0 R >>","<< /Type /Pages /Count "+sizes.length+" /Kids ["+sizes.map((_,i)=>(i+3)+" 0 R").join(" ")+"] >>",...sizes.map(([w,h])=>"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 "+w+" "+h+"] /Resources << >> >>")];let pdf="%PDF-1.7\n", offsets=[0];objects.forEach((o,i)=>{offsets.push(pdf.length);pdf+=(i+1)+" 0 obj\n"+o+"\nendobj\n";});const start=pdf.length;pdf+="xref\n0 "+(objects.length+1)+"\n0000000000 65535 f \n"+offsets.slice(1).map(o=>String(o).padStart(10,"0")+" 00000 n \n").join("")+"trailer\n<< /Size "+(objects.length+1)+" /Root 1 0 R >>\nstartxref\n"+start+"\n%%EOF\n";const transfer=new DataTransfer();for(const filename of Array.isArray(name)?name:[name])transfer.items.add(new File([pdf],filename,{type:"application/pdf"}));const input=document.querySelector("#file-input");input.files=transfer.files;input.dispatchEvent(new Event("change",{bubbles:true}));};window.polishDownloads=[];const urls=new Map(),create=URL.createObjectURL.bind(URL),click=HTMLAnchorElement.prototype.click;URL.createObjectURL=blob=>{const url=create(blob);urls.set(url,blob);return url;};HTMLAnchorElement.prototype.click=function(){if(this.download&&urls.has(this.href))polishDownloads.push({name:this.download,blob:urls.get(this.href)});return click.call(this);};true' | ignore

        upload mixed.pdf [[612 792] [595 842]]
        browser wait '.tool-form' | ignore
        browser find role button click --name 'Extract pages' --exact | ignore
        browser find role button click --name Combined --exact | ignore
        browser click '.tool-form button[type=submit]' | ignore
        browser wait '.export-notice' | ignore
        assert-browser 'polishDownloads.length===1 && polishDownloads[0].blob.size>0' 'Mixed-size extraction did not download'
        browser eval '(async()=>{const form=new FormData();form.append("file",polishDownloads[0].blob,"result.pdf");const r=await fetch("/pdf/inspect",{method:"POST",body:form});if(!r.ok||(await r.json()).pageCount!==2)throw new Error("Extracted mixed PDF was not readable");return true;})()' | ignore

        upload five.pdf [[360 360] [360 360] [360 360] [360 360] [360 360]]
        browser click '#extract-pages-range-trigger' | ignore
        browser fill '#extract-pages-range-input' '5' | ignore
        browser click '#extract-pages-range-apply' | ignore
        upload two.pdf [[360 360] [360 360]]
        browser wait --text 'Choose All pages or update the range.' | ignore
        assert-browser 'document.querySelector(".tool-form button[type=submit]").disabled' 'Stale range did not block export'
        browser find role button click --name 'All pages' --exact | ignore
        assert-browser '!document.querySelector(".tool-form button[type=submit]").disabled' 'All pages did not recover stale range'

        browser find role button click --name 'Impose artwork' --exact | ignore
        browser wait --text 'Continue to copies' | ignore
        browser wait --fn 'document.activeElement?.id==="finished-width"' | ignore
        assert-browser 'document.activeElement?.id==="finished-width"' 'Impose entry did not focus Finished width'
        assert-browser 'document.querySelector(".setup-continue-button").disabled && !document.querySelector("select[aria-label=\"Finished size\"] option[value=original]")' 'PDF source size was accepted without an explicit finished size'
        browser focus 'input[aria-label="Finished width (in)"]' | ignore
        browser press Control+a | ignore
        browser press Backspace | ignore
        browser keyboard type '4.25' | ignore
        browser focus 'input[aria-label="Finished height (in)"]' | ignore
        browser press Control+a | ignore
        browser press Backspace | ignore
        browser keyboard type '6.25' | ignore
        browser wait --fn 'document.querySelector("input[aria-label=\"Finished width (in)\"]").value==="4.25" && document.querySelector("input[aria-label=\"Finished height (in)\"]").value==="6.25" && !document.querySelector(".setup-continue-button").disabled' | ignore
        browser find role button click --name 'Continue to copies' --exact | ignore
        browser wait --fn '!!document.querySelector(".piece-cut")' | ignore
        assert-browser 'Array.from(document.querySelectorAll(".piece-cut")).some(e=>e.getAttribute("width")==="4.25" && e.getAttribute("height")==="6.25")' 'Decimal finished dimensions were not used by the rendered layout'
        browser eval 'document.querySelector("#gang-setup-panel .setup-stepper button:first-child")?.click();true' | ignore
        browser wait '#setup-step-pdf' | ignore
        browser fill 'input[aria-label="Finished width (in)"]' '' | ignore
        assert-browser 'document.querySelector(".setup-continue-button").disabled && document.querySelector("input[aria-label=\"Finished width (in)\"]").getAttribute("aria-invalid")==="true"' 'Invalid decimal finished input bypassed validation'
        browser fill 'input[aria-label="Finished width (in)"]' '5' | ignore
        browser fill 'input[aria-label="Finished height (in)"]' '' | ignore
        assert-browser 'document.querySelector(".setup-continue-button").disabled && document.querySelector("input[aria-label=\"Finished height (in)\"]").getAttribute("aria-invalid")==="true"' 'Missing height accepted a PDF finished size'
        browser fill 'input[aria-label="Finished height (in)"]' '5' | ignore
        browser wait --fn '!document.querySelector(".setup-continue-button").disabled' | ignore
        assert-artwork-context
        browser focus 'select[aria-label="Artwork fitting"]' | ignore
        assert-browser 'document.activeElement?.getAttribute("aria-label")==="Artwork fitting"' 'Artwork fitting was not keyboard focusable'
        browser press End | ignore
        browser wait --fn 'document.querySelector("select[aria-label=\"Artwork fitting\"]").value==="cover"' | ignore
        browser press Home | ignore
        browser wait --fn 'document.querySelector("select[aria-label=\"Artwork fitting\"]").value==="contain"' | ignore
        browser press ArrowDown | ignore
        browser wait --fn 'document.querySelector("select[aria-label=\"Artwork fitting\"]").value==="stretch"' | ignore
        browser click '.preset-toolbar-trigger' | ignore
        browser wait --fn 'document.querySelector(".preset-toolbar-trigger").getAttribute("aria-expanded")==="true" && document.querySelector("#preset-panel")?.checkVisibility()' | ignore
        browser wait --fn '!document.querySelector("#preset-select").disabled && !!document.querySelector("#preset-select option[value=\"5x7-on-12x18\"]")' | ignore
        assert-browser 'document.querySelector(".preset-toolbar")?.closest(".impose-workspace-toolbar-controls")' 'Toolbar Presets did not open in the shared controls row'
        browser focus '#preset-select' | ignore
        assert-browser 'document.activeElement?.id==="preset-select"' 'Preset controls were not keyboard focusable'
        browser select '#preset-select' '5x7-on-12x18' | ignore
        browser find role button click --name 'Apply preset' --exact | ignore
        browser wait --fn 'document.querySelector("input[aria-label=\"Finished width (in)\"]").value==="5" && document.querySelector("input[aria-label=\"Finished height (in)\"]").value==="7" && !document.querySelector(".setup-continue-button").disabled' | ignore
        browser fill 'input[aria-label="Finished width (in)"]' '' | ignore
        assert-browser 'document.querySelector("input[aria-label=\"Finished width (in)\"]").getAttribute("aria-invalid")==="true" && document.querySelector(".setup-continue-button").disabled' 'Invalid finished-size draft did not block before same-value preset recovery'
        browser find role button click --name 'Apply preset' --exact | ignore
        browser wait --fn 'document.querySelector("input[aria-label=\"Finished width (in)\"]").value==="5" && document.querySelector("input[aria-label=\"Finished height (in)\"]").value==="7" && document.querySelector("input[aria-label=\"Finished width (in)\"]").getAttribute("aria-invalid")==="false" && !document.querySelector("input[aria-label=\"Finished width (in)\"]").hasAttribute("aria-describedby") && !document.querySelector(".setup-continue-button").disabled' | ignore
        assert-browser 'document.querySelector("#preset-panel [role=status]")?.textContent==="Applied 5x7 on 12x18."' 'Same-value preset did not report successful application'
        browser click '.preset-toolbar-trigger' | ignore
        browser wait --fn 'document.querySelector(".preset-toolbar-trigger").getAttribute("aria-expanded")==="false" && !document.querySelector("#preset-panel")' | ignore
        browser click '#gang-setup-tab' | ignore
        # The finished-size controls put Repeat below the sticky action bar.
        # Scroll the real target into view rather than clicking the obscured center.
        browser eval 'document.querySelector(".impose-mode-button").scrollIntoView({block:"center",behavior:"instant"});true' | ignore
        assert-browser '(()=>{const e=document.querySelector(".impose-mode-button"),r=e.getBoundingClientRect();return e.contains(document.elementFromPoint(r.x+r.width/2,r.y+r.height/2));})()' 'Repeat is obscured after scrolling into view'
        browser find role button click --name 'Repeat pages' | ignore
        assert-browser '!!document.querySelector("#setup-step-pdf")' 'Selecting Repeat advanced the wizard'
        assert-browser 'document.querySelector(".impose-mode-button").getAttribute("aria-pressed")==="true"' 'Repeat was not selected'
        browser find role button click --name 'Continue to copies' --exact | ignore
        assert-artwork-context
        browser click '#gang-setup-tab' | ignore
        browser wait --fn 'document.querySelector(".impose-result-summary")?.textContent.includes("per sheet")' | ignore
        assert-browser 'document.querySelector(".impose-result-summary")?.textContent.includes("required")' 'Copies did not expose the derived sheet summary'
        browser click '#impose-controls-toggle' | ignore
        assert-browser 'document.querySelector(".gang-workspace").classList.contains("controls-collapsed") && getComputedStyle(document.querySelector(".gang-setup")).display==="none" && document.querySelector("#impose-controls-toggle").getAttribute("aria-expanded")==="false"' 'Desktop divider control did not collapse the setup rail'
        browser click '#impose-controls-toggle' | ignore
        assert-browser '!document.querySelector(".gang-workspace").classList.contains("controls-collapsed") && getComputedStyle(document.querySelector(".gang-setup")).display!=="none" && document.querySelector("#impose-controls-toggle").getAttribute("aria-expanded")==="true"' 'Desktop divider control did not restore the setup rail'
        browser select '.impose-sheet-controls select' custom | ignore
        browser fill 'input[aria-label="Sheet width (in)"]' '' | ignore
        assert-browser 'document.querySelector(".setup-continue-button").disabled && document.querySelector("input[aria-label=\"Sheet width (in)\"]").getAttribute("aria-invalid")==="true"' 'Empty dimension submitted its old value'
        browser type 'input[aria-label="Sheet width (in)"]' '5.25' | ignore
        assert-browser 'document.querySelector("input[aria-label=\"Sheet width (in)\"]").value==="5.25"' 'Numeric validation interrupted decimal typing'
        browser fill 'input[aria-label="Sheet width (in)"]' '' | ignore
        browser click '#edit-copy-quantities' | ignore
        assert-browser 'document.querySelector("#quantity-editor-done").getAttribute("aria-disabled")==="false"' 'An unrelated dimension error blocked the valid quantity dialog'
        browser click '#quantity-editor-done' | ignore
        assert-browser 'document.querySelector("input[aria-label=\"Sheet width (in)\"]").value==="" && document.querySelector(".setup-continue-button").disabled' 'Closing the quantity dialog cleared an unrelated invalid dimension'
        browser fill 'input[aria-label="Sheet width (in)"]' '13' | ignore
        browser click '#edit-copy-quantities' | ignore
        browser fill 'input[aria-label="Page 1 copies"]' '' | ignore
        assert-browser 'document.querySelector(".setup-continue-button").disabled' 'An invalid quantity was missing from overall workflow validation'
        browser press Escape | ignore
        browser wait --fn '!document.querySelector("#quantity-editor-dialog")' | ignore
        assert-browser '!document.querySelector(".setup-continue-button").disabled' 'Closing the invalid quantity draft left workflow validation blocked'
        browser click '#edit-copy-quantities' | ignore
        assert-browser 'document.querySelector("input[aria-label=\"Page 1 copies\"]").value==="1" && document.querySelector("#quantity-editor-done").getAttribute("aria-disabled")==="false"' 'Reopening quantities retained a discarded invalid draft'
        browser fill '#bulk-copy-quantity' '-1' | ignore
        assert-browser 'document.querySelector("#quantity-editor-done").getAttribute("aria-disabled")==="true"' 'Invalid copy draft could be accepted'
        assert-browser 'document.querySelector(".quantity-bulk-editor button").disabled && !!document.querySelector("#quantity-editor-dialog") && document.querySelector("input[aria-label=\"Page 1 copies\"]").value==="1"' 'Invalid bulk quantity enabled Apply, closed the dialog or changed copies'
        browser fill '#bulk-copy-quantity' '0' | ignore
        browser focus '.quantity-bulk-editor button' | ignore
        browser press Enter | ignore
        browser wait --fn '!document.querySelector("#quantity-editor-dialog") && document.activeElement.id==="edit-copy-quantities"' | ignore
        assert-browser 'document.querySelector(".setup-continue-button").disabled' 'Zero total copies enabled workflow progression'
        browser click '#edit-copy-quantities' | ignore
        assert-browser 'document.querySelector("input[aria-label=\"Page 1 copies\"]").value==="0" && document.querySelector("input[aria-label=\"Page 2 copies\"]").value==="0" && document.querySelector(".quantity-dialog-total strong").textContent==="0" && document.querySelector(".setup-continue-button").disabled' 'Keyboard Apply did not persist zero quantities or zero total remained actionable'
        browser fill '#bulk-copy-quantity' '37' | ignore
        browser find role button click --name 'Apply to all' --exact | ignore
        browser wait --fn '!document.querySelector("#quantity-editor-dialog") && document.activeElement.id==="edit-copy-quantities"' | ignore
        browser click '#edit-copy-quantities' | ignore
        assert-browser 'document.querySelector("input[aria-label=\"Page 1 copies\"]").value==="37" && document.querySelector("input[aria-label=\"Page 2 copies\"]").value==="37"' 'Bulk copies were not persisted after reopening'
        browser fill 'input[aria-label="Page 1 copies"]' '' | ignore
        assert-browser 'document.querySelector("#quantity-editor-done").getAttribute("aria-disabled")==="true"' 'Empty individual copy draft could be accepted'
        browser find role button click --name 'Apply to all' --exact | ignore
        browser wait --fn '!document.querySelector("#quantity-editor-dialog") && document.activeElement.id==="edit-copy-quantities"' | ignore
        browser click '#edit-copy-quantities' | ignore
        assert-browser 'document.querySelector("input[aria-label=\"Page 1 copies\"]").value==="37" && document.querySelector("#quantity-editor-done").getAttribute("aria-disabled")==="false"' 'Applying an unchanged bulk quantity did not replace the invalid individual draft'
        browser fill 'input[aria-label="Page 1 copies"]' '' | ignore
        browser fill 'input[aria-label="Page 1 copies"]' '10001' | ignore
        assert-browser 'document.querySelector("input[aria-label=\"Page 1 copies\"]").value==="10001" && document.querySelector("#quantity-editor-done").getAttribute("aria-disabled")==="true"' 'Excessive individual copies were silently clamped'
        browser fill 'input[aria-label="Page 1 copies"]' '37' | ignore
        browser focus '#quantity-editor-close' | ignore
        browser press Shift+Tab | ignore
        assert-browser 'document.activeElement.id==="quantity-editor-done"' 'Copy editor reverse focus escaped'
        browser click '#quantity-editor-done' | ignore
        browser wait --fn 'document.activeElement.id==="edit-copy-quantities"' | ignore
        browser select '.printing-control select' double | ignore
        browser click '#edit-copy-quantities' | ignore
        browser fill '#bulk-copy-quantity' '37' | ignore
        browser find role button click --name 'Apply to all' --exact | ignore
        browser wait --fn '!document.querySelector("#quantity-editor-dialog") && document.activeElement.id==="edit-copy-quantities"' | ignore
        browser click '#edit-copy-quantities' | ignore
        assert-browser 'document.querySelector("input[aria-label=\"Pages 1–2 copies\"]").value==="37" && document.querySelector(".quantity-dialog-total strong").textContent==="37"' 'Duplex copies did not use page-pair quantities'
        browser click '#quantity-editor-done' | ignore
        browser find role button click --name 'Continue to layout' --exact | ignore
        assert-artwork-context
        browser click '#gang-setup-tab' | ignore
        browser wait --fn 'document.querySelector(".layout-result-summary")?.textContent.includes("grid")' | ignore
        assert-browser 'document.querySelector(".layout-result-summary")?.textContent.includes("per sheet")' 'Layout did not expose the derived grid summary'
        browser find role button click --name 'Custom grid' | ignore
        browser fill 'input[aria-label="Rows"]' '1.5' | ignore
        assert-browser 'document.querySelector(".setup-continue-button").disabled' 'Fractional grid submitted a prior integer'
        browser fill 'input[aria-label="Rows"]' '2' | ignore
        browser fill 'input[aria-label="Columns"]' '2' | ignore
        browser click '.advanced-layout-details > button' | ignore
        assert-browser '!!document.querySelector("#setup-step-layout")' 'Advanced settings submitted the step'
        browser wait --fn '!document.querySelector(".setup-continue-button").disabled' | ignore
        browser find role button click --name 'Continue to bleed' --exact | ignore
        assert-artwork-context
        browser click '#gang-setup-tab' | ignore
        browser wait --fn '!document.querySelector("#gang-download-pdf").disabled' | ignore
        browser click '#gang-download-pdf' | ignore
        browser wait '.export-notice' | ignore
        assert-browser 'polishDownloads.length===2 && polishDownloads[1].blob.size>0' 'Repeat imposition did not download'
        browser set viewport 800 900 | ignore
        assert-browser 'document.documentElement.scrollWidth<=innerWidth' 'Half-screen workflow overflows horizontally'
        assert-browser 'getComputedStyle(document.querySelector("#impose-controls-toggle")).display==="none"' 'Desktop collapse control remained visible in the tabbed narrow layout'
        browser click '#gang-artwork-tab' | ignore
        browser wait --fn 'document.querySelector("#gang-artwork-panel")?.checkVisibility() && document.querySelector("#gang-artwork-panel .impose-artwork-controls")?.checkVisibility()' | ignore
        assert-browser '(()=>{const panel=document.querySelector("#gang-artwork-panel"),r=panel?.getBoundingClientRect();return !!r && r.top>=0 && r.bottom<=innerHeight+1 && r.left>=0 && r.right<=innerWidth+1;})()' 'Narrow Artwork rail is clipped'
        browser click '#gang-preview-tab' | ignore
        assert-browser '(()=>{const svg=document.querySelector(".sheet-svg"),r=svg.getBoundingClientRect();return r.width>=200 && r.height>=200 && r.top>=0 && r.bottom<=innerHeight+1 && r.left>=0 && r.right<=innerWidth+1 && document.querySelector(".impose-workspace-toolbar")?.checkVisibility() && document.querySelector("#gang-preview-tab").getAttribute("aria-selected")==="true";})()' 'Narrow Preview is clipped or lost the workspace Artwork toolbar'
        browser click '#gang-setup-tab' | ignore
        assert-artwork-context
        browser find role button click --name 'PDF to images' --exact | ignore
        browser wait '.tool-form' | ignore
        browser eval 'polishUpload(["first.pdf","second.pdf","third.pdf"],[[360,360]])' | ignore
        browser wait --fn 'document.querySelectorAll(".file-row").length===3' | ignore
        browser focus '.file-row:first-child' | ignore
        browser eval 'window.movedRow=document.activeElement.id' | ignore
        browser press Alt+ArrowDown | ignore
        browser wait --fn 'document.activeElement.id===movedRow && document.querySelectorAll(".file-row")[1].id===movedRow' | ignore
        browser press Alt+ArrowDown | ignore
        browser wait --fn 'document.activeElement.id===movedRow && document.querySelectorAll(".file-row")[2].id===movedRow' | ignore
        browser find role button click --name 'Combine PDFs' --exact | ignore
        browser eval 'window.polishXhrSend=XMLHttpRequest.prototype.send;XMLHttpRequest.prototype.send=function(){queueMicrotask(()=>this.dispatchEvent(new ProgressEvent("error")));};true' | ignore
        browser click '.tool-form button[type=submit]' | ignore
        browser wait '.tool-form [role=alert]' | ignore
        assert-browser 'document.querySelectorAll(".file-row").length===3 && polishDownloads.length===2' 'Failed upload discarded files or downloaded a result'
        browser eval 'XMLHttpRequest.prototype.send=polishXhrSend;true' | ignore
        browser eval 'window.polishOriginalFetch=window.fetch;window.polishPollCount=0;window.fetch=(request,...args)=>request.url?.includes("/jobs/")&&request.method==="GET" ? new Promise((resolve,reject)=>{window.polishPollCount++;request.signal.addEventListener("abort",()=>reject(new DOMException("Aborted","AbortError")),{once:true});}) : polishOriginalFetch(request,...args);true' | ignore
        browser click '.tool-form button[type=submit]' | ignore
        browser wait --fn 'polishPollCount===1' | ignore
        browser find role button click --name Cancel --exact | ignore
        browser wait --fn '!document.querySelector(".tool-form button[type=submit]").disabled' | ignore
        assert-browser 'document.querySelectorAll(".file-row").length===3 && polishDownloads.length===2' 'Cancellation discarded files or downloaded a result'
        browser eval 'window.fetch=polishOriginalFetch;true' | ignore
        browser click '.tool-form button[type=submit]' | ignore
        browser wait '.export-notice' | ignore
        assert-browser 'polishDownloads.length===3 && polishDownloads[2].blob.size>0' 'Reordered merge did not download'
        browser find role button click --name 'PDF to images' --exact | ignore
        browser click '.tool-form button[type=submit]' | ignore
        browser wait '.export-notice' | ignore
        assert-browser 'polishDownloads.length===4 && polishDownloads[3].name.endsWith(".zip")' 'Multi-PDF image export did not download'
        browser eval '(async()=>{const bytes=new Uint8Array(await polishDownloads[3].blob.arrayBuffer());if(bytes[0]!==80||bytes[1]!==75)throw new Error("Raster download is not a ZIP");return true;})()' | ignore

        browser eval '(async()=>{const canvas=document.createElement("canvas");canvas.width=240;canvas.height=120;const ctx=canvas.getContext("2d");ctx.fillStyle="#176b9e";ctx.fillRect(0,0,240,120);const blob=await new Promise(resolve=>canvas.toBlob(resolve,"image/png"));const transfer=new DataTransfer();transfer.items.add(new File([blob],"artwork.png",{type:"image/png"}));canvas.width=120;canvas.height=240;ctx.fillStyle="#9e6b17";ctx.fillRect(0,0,120,240);const portrait=await new Promise(resolve=>canvas.toBlob(resolve,"image/png"));transfer.items.add(new File([portrait],"portrait.png",{type:"image/png"}));const input=document.querySelector("#file-input");input.files=transfer.files;input.dispatchEvent(new Event("change",{bubbles:true}));return true;})()' | ignore
        browser wait --text 'Each page matches its image at 300 DPI.' | ignore
        assert-browser '!document.querySelector(".tool-form [aria-label=\"PDF page size\"], .tool-form [aria-label=\"PDF page orientation\"], .tool-form [aria-label=\"Image fit\"], #image-pdf-margin") && !document.querySelector(".tool-form button[type=submit]").disabled' 'Images to PDF still exposes geometry settings or blocks original-size output'
        browser eval 'window.polishImageRequest=null;const imageSend=XMLHttpRequest.prototype.send;XMLHttpRequest.prototype.send=function(body){if(body instanceof FormData && body.get("target")==="pdf"){polishImageRequest=Array.from(body.entries()).filter(([,value])=>typeof value==="string");}return imageSend.call(this,body);};true' | ignore
        browser click '.tool-form button[type=submit]' | ignore
        browser wait '.export-notice' | ignore
        assert-browser 'polishDownloads.length===5 && polishDownloads[4].name.endsWith(".pdf")' 'Original-size image conversion did not download'
        assert-browser 'JSON.stringify(polishImageRequest)===JSON.stringify([["action","convert"],["target","pdf"],["layout","single"],["pageSize","original"]])' 'Images to PDF did not send the original-size single-image request contract'
        browser eval '(async()=>{const form=new FormData();form.append("file",polishDownloads[4].blob,"image-output.pdf");const r=await fetch("/gang-up/analyze",{method:"POST",body:form});if(!r.ok)throw new Error("Image PDF could not be inspected");const result=await r.json();if(result.pageCount!==2||result.sourcePages.length!==2||result.sourcePages.some((page,index)=>{const [width,height]=index===0?[0.8,0.4]:[0.4,0.8];return Math.abs(page.sourcePdfSize.width-width)>0.0001||Math.abs(page.sourcePdfSize.height-height)>0.0001;}))throw new Error("Image PDF lost original-size 300 DPI geometry or image order");return true;})()' | ignore
        print 'All five workflows downloaded successfully; mixed geometry, range recovery, numeric drafts, modal focus, repeated keyboard reorder, failed-upload retry, cancellation, and half-screen layout passed.'
        null
    } catch {|error| $error }
    if $result != null { print (browser snapshot); print (browser errors) }
    browser close | ignore
    if $result != null { error make $result.raw }
}
