#!/usr/bin/env nu

const project_dir = path self ..

def fail [message: string] {
    error make { msg: $"pdf-app dev: ($message)" }
}

def run-command [command: closure] {
    let result = do $command | complete
    if not ($result.stdout | is-empty) { print --no-newline $result.stdout }
    if not ($result.stderr | is-empty) { print --stderr --no-newline $result.stderr }
    if $result.exit_code != 0 {
        fail $"command exited with status ($result.exit_code)"
    }
}

def require-command [name: string, requirement: string] {
    if (which $name | is-empty) { fail $requirement }
}

def job-running [job_id: int] {
    not ((job list | where id == $job_id) | is-empty)
}

def stop-job [job_id: int] {
    if (job-running $job_id) {
        job kill $job_id
    }
}

def main [] {
    let frontend_dir = $project_dir | path join frontend
    let cargo_target = $env.CARGO_TARGET_DIR? | default ($project_dir | path join target)
    let cargo_target_dir = if ($cargo_target | str starts-with "/") {
        $cargo_target
    } else {
        $project_dir | path join $cargo_target
    }
    let backend_binary = $cargo_target_dir | path join debug pdf-tools-server
    let backend_host = "127.0.0.1"
    let backend_port = 3200
    let base_url = $"http://($backend_host):($backend_port)"
    let pdfium_path = $env.PDF_TOOLS_PDFIUM_PATH? | default ""

    require-command cargo "cargo is required"
    require-command rustup "rustup is required"
    require-command cargo-leptos "cargo-leptos 0.3.7 is required"

    let leptos_version = do { ^cargo leptos --version } | complete
    if $leptos_version.exit_code != 0 or ($leptos_version.stdout | str trim) != "cargo-leptos 0.3.7" {
        fail "cargo-leptos 0.3.7 is required"
    }
    let rust_toolchain = do { ^rustup run 1.97.1 rustc --version } | complete
    if $rust_toolchain.exit_code != 0 { fail "Rust toolchain 1.97.1 is required" }
    let installed_targets = do { ^rustup target list --toolchain 1.97.1 --installed } | complete
    if $installed_targets.exit_code != 0 or "wasm32-unknown-unknown" not-in ($installed_targets.stdout | lines) {
        fail "Rust toolchain 1.97.1 requires the wasm32-unknown-unknown target"
    }
    if not ($pdfium_path | is-empty) and not ($pdfium_path | path exists) {
        fail $"PDF_TOOLS_PDFIUM_PATH does not name a file: ($pdfium_path)"
    }
    let selected_port = try { port $backend_port $backend_port } catch { null }
    if $selected_port == null {
        fail $"port ($backend_port) is already in use; stop the existing service before starting PDF Tools"
    }

    print "Building PDF Tools backend…"
    run-command { cd $project_dir; ^cargo +1.97.1 build --locked --bin pdf-tools-server }

    mut backend_job = -1
    mut frontend_job = -1
    let failures = try {
        $backend_job = job spawn --description "pdf-app backend" {
            cd $project_dir
            with-env {
                PORT: ($backend_port | into string)
                PDF_TOOLS_BIND_ADDRESS: $backend_host
            } {
                ^$backend_binary
            }
        }

        mut backend_ready = false
        for _ in 1..100 {
            if not (job-running $backend_job) {
                fail "backend exited before becoming ready; review the error above"
            }
            $backend_ready = try {
                http get --raw --max-time 1sec $"($base_url)/health" | ignore
                true
            } catch {
                false
            }
            if $backend_ready { break }
            sleep 100ms
        }
        if not $backend_ready {
            fail $"backend did not become ready at ($base_url)/health"
        }

        $frontend_job = job spawn --description "pdf-app cargo-leptos" {
            cd $frontend_dir
            with-env { NO_COLOR: "true", RUSTUP_TOOLCHAIN: "1.97.1" } {
                ^cargo leptos watch --frontend-only --split --lib-cargo-args=--locked
            }
        }

        mut frontend_ready = false
        for _ in 1..1200 {
            if not (job-running $backend_job) {
                fail "backend exited while cargo-leptos was starting"
            }
            if not (job-running $frontend_job) {
                fail "cargo-leptos exited before becoming ready; review the error above"
            }
            let assets_exist = (($frontend_dir | path join dist app.html) | path exists) and (($frontend_dir | path join dist pkg pdf-tools-frontend.js) | path exists)
            if $assets_exist {
                $frontend_ready = try {
                    http get --max-time 1sec $"($base_url)/" | ignore
                    true
                } catch {
                    false
                }
            }
            if $frontend_ready { break }
            sleep 100ms
        }
        if not $frontend_ready {
            fail $"cargo-leptos did not produce a frontend at ($base_url)/"
        }
        try {
            http get --max-time 2sec $"($base_url)/pkg/pdf-tools-frontend.js" | ignore
        } catch {
            fail "cargo-leptos JavaScript output was not served"
        }

        print $"PDF Tools is ready at ($base_url)/"
        loop {
            if not (job-running $backend_job) {
                fail "backend exited unexpectedly"
            }
            if not (job-running $frontend_job) {
                fail "cargo-leptos exited unexpectedly"
            }
            sleep 200ms
        }
        []
    } catch {|error|
        [$error]
    }

    if $frontend_job >= 0 { stop-job $frontend_job }
    if $backend_job >= 0 { stop-job $backend_job }
    if not ($failures | is-empty) {
        print --stderr $failures.0.msg
        exit 1
    }
}
