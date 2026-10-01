#!/usr/bin/env nu

const frontend_dir = path self ..

def main [site_root?: path] {
    let package_dir = $site_root | default ($frontend_dir | path join dist) | path join pkg
    if not ($package_dir | path exists) {
        error make { msg: "frontend dist package directory is missing; build the frontend first" }
    }

    let assets = [css js wasm]
        | each {|extension| glob ($package_dir | path join $"**/*.($extension)") }
        | flatten
    if ($assets | is-empty) {
        error make { msg: "frontend dist does not contain compressible assets" }
    }

    for asset in $assets {
        ^gzip --best --force --keep --no-name $asset
        if $env.LAST_EXIT_CODE != 0 {
            error make { msg: $"could not compress frontend asset: ($asset)" }
        }
    }
}
