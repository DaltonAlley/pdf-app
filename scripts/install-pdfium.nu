#!/usr/bin/env nu

def fail [message: string] {
    error make { msg: $message }
}

def run-command [command: closure] {
    let result = do $command | complete
    if not ($result.stdout | is-empty) { print --no-newline $result.stdout }
    if not ($result.stderr | is-empty) { print --stderr --no-newline $result.stderr }
    if $result.exit_code != 0 {
        fail $"command exited with status ($result.exit_code)"
    }
}

def main [destination: path, architecture?: string] {
    let selected_architecture = $architecture | default (^uname -m | str trim)
    let artifact = match $selected_architecture {
        "amd64" | "x86_64" => {
            archive: "pdfium-linux-x64.tgz"
            checksum: "1470e21b8b4a3b4ad7f85684e2da11d94f3b69a86d81dee11b9b6709d927ac1d"
        }
        "arm64" | "aarch64" => {
            archive: "pdfium-linux-arm64.tgz"
            checksum: "ee7f7b7d5468958336a818c1cd580bdd20972846b7377b13f9a923d92d1d4674"
        }
        _ => { fail $"unsupported PDFium architecture: ($selected_architecture)" }
    }

    mkdir $destination
    let temporary = $nu.temp-dir | path join $"pdfium-(random uuid).tgz"
    try {
        (http get
            --max-time 5min
            $"https://github.com/bblanchon/pdfium-binaries/releases/download/chromium%2F7881/($artifact.archive)")
            | save --force $temporary
        let actual_checksum = open --raw $temporary | hash sha256
        if $actual_checksum != $artifact.checksum {
            fail $"PDFium archive checksum mismatch: expected ($artifact.checksum), got ($actual_checksum)"
        }
        run-command { ^tar -xzf $temporary -C $destination }
    } catch {|error|
        rm --force $temporary
        print --stderr $error.msg
        exit 1
    }
    rm --force $temporary
    print ($destination | path expand | path join lib libpdfium.so)
}
