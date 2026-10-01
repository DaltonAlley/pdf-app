# PDF inspection edge-case audit

This agent-browser audit used separate subagents for imposition and conversion
workflows, with the parent auditing inspection cancellation and integrating fixes.

## Confirmed issues

1. **Cancel did nothing during PDF inspection.** In a valid generic workspace,
   replace the source while `/gang-up/analyze` is delayed, then click Cancel.
   The inspection kept running and all workspace navigation remained disabled.
   Reproduced twice with a controlled pending request. Inspection now shares the
   existing cancellation controller, stops the remaining batch, and checks for
   cancellation again before committing a response. Previous files and page counts
   remain intact. Initial upload and imposition also expose Cancel inspection.
2. **Imposition hid rejected replacement errors.** With valid artwork on Copies,
   select a corrupt replacement PDF. The previous artwork and settings correctly
   remained, but no error appeared. The imposition subagent reproduced this twice.
   The parent selection error now appears as an alert in the imposition workspace.

## Coverage

`scripts/inspection-browser-smoke.nu` exercises initial upload, generic and
imposition cancellation, stopping a multi-file inspection after its first request,
late successful responses after cancellation, retained source, successful retry,
and visible corrupt-replacement errors without losing the imposition workspace.
The current smoke intercepts `/pdf/inspect`, the general intake endpoint that
replaced imposition analysis for readability checks.
All four scenarios passed against the release build. The alert's bounds are
checked above the workspace; visual checks also passed at 1280 and 800 pixels
wide. The existing startup browser smoke passed, including deferred module
retry, file identity/settings, and CSS/JavaScript/Wasm startup recovery.

The conversion subagent checked page bounds, invalid and valid chunk sizes,
offline extraction followed by online retry, mixed-size merge, keyboard file
ordering/focus, image page geometry/margins, and corrupt replacement recovery.
The imposition subagent checked trim/grid boundaries and correction, Repeat mode,
odd/even duplex availability, paired copy totals, and advanced disclosures.

Two candidates were excluded after investigation: native dialog focus briefly
visiting browser chrome without entering background controls, and an automation
click hitting the fixed footer while its intended disclosure was outside the
inner scroll area. Neither was treated as an application defect.

This is bounded browser coverage, not exhaustive PDF-format verification.
Conversion exports in the subagent pass were checked through completed HTTP 200
downloads; local output bytes were not independently inspected in this pass.
