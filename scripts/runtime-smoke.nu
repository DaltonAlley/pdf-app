#!/usr/bin/env nu

const project_dir = path self ..

def fail [message: string] {
    error make { msg: $"pdf-app runtime smoke: ($message)" }
}

def run-command [command: closure] {
    let result = do $command | complete
    if not ($result.stdout | is-empty) { print --no-newline $result.stdout }
    if not ($result.stderr | is-empty) { print --stderr --no-newline $result.stderr }
    if $result.exit_code != 0 {
        fail $"command exited with status ($result.exit_code)"
    }
}

def job-running [job_id: int] {
    not ((job list | where id == $job_id) | is-empty)
}

def stop-job [job_id: int] {
    if (job-running $job_id) {
        job kill $job_id
    }
}

def available-port [] {
    let requested = $env.PDF_TOOLS_SMOKE_PORT? | default ""
    if not ($requested | is-empty) {
        let parsed = try { $requested | into int } catch { -1 }
        if $parsed < 1 or $parsed > 65535 {
            fail "PDF_TOOLS_SMOKE_PORT must be between 1 and 65535"
        }
        let available = try { port $parsed $parsed } catch { null }
        if $available == null {
            fail $"selected port ($parsed) is already in use"
        }
        $parsed
    } else {
        port 31000 51000
    }
}

def main [] {
    let dist_dir = $project_dir | path join frontend dist
    let cargo_target = $env.CARGO_TARGET_DIR? | default ($project_dir | path join target)
    let cargo_target_dir = if ($cargo_target | str starts-with "/") {
        $cargo_target
    } else {
        $project_dir | path join $cargo_target
    }
    let backend_binary = $cargo_target_dir | path join release pdf-tools-server
    let pdfium_path = $env.PDF_TOOLS_PDFIUM_PATH? | default ""

    if not (($dist_dir | path join app.html) | path exists) {
        fail "frontend/dist/app.html is missing; run the release cargo-leptos build first"
    }
    if not ($pdfium_path | is-empty) and not ($pdfium_path | path exists) {
        fail $"PDF_TOOLS_PDFIUM_PATH does not name a file: ($pdfium_path)"
    }

    mkdir $cargo_target_dir
    let smoke_dir = ^mktemp -d ($cargo_target_dir | path join "runtime-smoke.XXXXXX") | str trim
    let data_dir = $smoke_dir | path join data
    let server_log = $smoke_dir | path join server.log
    mkdir $data_dir

    mut server_job = -1
    let failures = try {
        run-command { cd $project_dir; ^cargo +1.97.1 build --release --locked --bin pdf-tools-server }

        let host = "127.0.0.1"
        let selected_port = available-port
        let base_url = $"http://($host):($selected_port)"
        $server_job = job spawn --description "pdf-app runtime smoke server" {
            cd $project_dir
            with-env {
                PORT: ($selected_port | into string)
                PDF_TOOLS_BIND_ADDRESS: $host
                PDF_TOOLS_DATA_DIR: $data_dir
            } {
                ^$backend_binary out+err> $server_log
            }
        }

        mut health = null
        for _ in 1..200 {
            if not (job-running $server_job) {
                fail "server exited before becoming ready"
            }
            $health = try {
                http get --raw --max-time 1sec $"($base_url)/health"
            } catch {
                null
            }
            if $health == "ok" { break }
            sleep 100ms
        }
        if $health != "ok" {
            fail "health endpoint did not return the expected response"
        }

        let index = http get --raw --max-time 5sec $"($base_url)/"
        if not ($index | str contains "<title>PDF Tools</title>") {
            fail "frontend entry point was not served"
        }

        let entry_assets = $index
            | parse --regex '/(?P<name>pkg/[a-f0-9]{64}/pdf-tools-frontend\.[A-Za-z0-9_-]+\.(?:js|wasm|css))'
            | get name
            | uniq
        if ($entry_assets | length) != 3 {
            fail "frontend entry point must reference versioned JavaScript, Wasm and CSS"
        }
        let hashes = open --raw ($dist_dir | path join asset-hashes.txt)
            | lines | parse "{kind}: {hash}"
        let manifest_hash = $hashes | where kind == manifest | get 0.hash
        let loader_hash = $hashes | where kind == split | get 0.hash
        let package_prefix = $entry_assets.0 | path dirname
        let manifest = open ($dist_dir | path join $package_prefix $"__wasm_split_manifest.($manifest_hash).json")
        if not ("generic_tools" in ($manifest | columns)) or not ("imposition" in ($manifest | columns)) {
            fail "release is missing a deferred workflow"
        }
        let deferred_assets = $manifest | values | flatten | uniq
            | each {|name| $"($package_prefix)/($name).wasm" }
        let assets = $entry_assets
            | append $"($package_prefix)/__wasm_split.($loader_hash).js"
            | append $deferred_assets
        for name in $assets {
            if not (($dist_dir | path join $name) | path exists) {
                fail $"referenced release asset is missing: ($name)"
            }
            let served = http get --max-time 5sec $"($base_url)/($name)"
            if ($served | is-empty) {
                fail $"served asset is empty: ($name)"
            }
            let original = $dist_dir | path join $name
            let sidecar = $"($original).gz"
            if not ($sidecar | path exists) {
                fail $"compressed release asset is missing: ($name).gz"
            }
            let downloaded = $smoke_dir | path join asset.gz
            let response_headers = $smoke_dir | path join asset.headers
            # Capture raw headers and bytes together: the built-in HTTP client
            # transparently decodes gzip and removes its Content-Encoding header.
            run-command { ^curl --fail --silent --show-error --max-time 5 --header "Accept-Encoding: gzip" --dump-header $response_headers --output $downloaded $"($base_url)/($name)" }
            let headers = open --raw $response_headers | lines | each {|line| $line | str trim | str downcase }
            if "content-encoding: gzip" not-in $headers {
                fail $"release asset was not served with gzip encoding: ($name)"
            }
            if (open --raw $downloaded | hash sha256) != (open --raw $sidecar | hash sha256) {
                fail $"served gzip bytes differ from the release sidecar: ($name)"
            }
            run-command { ^gzip --test $downloaded }
            let decoded = ^gzip --decompress --stdout $downloaded | hash sha256
            if $decoded != (open --raw $original | hash sha256) {
                fail $"compressed release asset does not decode to its original bytes: ($name)"
            }
        }

        let api_response = (http post
            --allow-errors
            --full
            --content-type application/json
            --max-time 5sec
            $"($base_url)/gang-up/layout"
            "not-json")
        if $api_response.status not-in [400 422] {
            fail $"invalid API input returned HTTP ($api_response.status) instead of a validation response"
        }
        if ($api_response.body | into string | str contains "<title>PDF Tools</title>") {
            fail "API validation request incorrectly fell back to the frontend"
        }

        let fixtures = $smoke_dir | path join fixtures
        run-command { ^nu --no-config-file ($project_dir | path join scripts smoke-fixtures.nu) $fixtures }
        print "Checking startup and deferred-module recovery..."
        run-command { ^nu --no-config-file ($project_dir | path join scripts startup-browser-smoke.nu) ($fixtures | path join source.pdf) --url $base_url }
        print "Checking inspection cancellation and replacement recovery..."
        run-command { ^nu --no-config-file ($project_dir | path join scripts inspection-browser-smoke.nu) ($fixtures | path join source.pdf) ($fixtures | path join replacement.pdf) ($fixtures | path join corrupt.pdf) --url $base_url }
        print "Checking loading and narrow-layout transitions..."
        run-command { ^nu --no-config-file ($project_dir | path join scripts transition-browser-smoke.nu) ($fixtures | path join source.pdf) ($fixtures | path join replacement.pdf) --url $base_url }
        print "Checking all five download workflows..."
        run-command { ^nu --no-config-file ($project_dir | path join scripts workflow-browser-smoke.nu) --url $base_url }
        print "Checking mixed artwork, finished sizing, crop, bleed and exported pixels..."
        run-command { ^nu --no-config-file ($project_dir | path join scripts mixed-artwork-browser-smoke.nu) ($fixtures | path join mixed.pdf) --bleed-pdf ($fixtures | path join manual-bleed.pdf) --url $base_url }
        print $"PDF Tools runtime smoke passed on ($host):($selected_port)"
        []
    } catch {|error|
        [$error]
    }

    if $server_job >= 0 { stop-job $server_job }
    if not ($failures | is-empty) {
        if ($server_log | path exists) {
            print --stderr "--- server log ---"
            open --raw $server_log | lines | last 80 | str join (char nl) | print --stderr
        }
        rm --recursive --force $smoke_dir
        print --stderr $failures.0.msg
        exit 1
    }
    rm --recursive --force $smoke_dir
}
