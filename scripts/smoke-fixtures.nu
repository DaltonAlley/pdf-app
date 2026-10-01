#!/usr/bin/env nu

# ASCII-only PDFs keep byte offsets deterministic and carry visible page identity.
def pdf [sizes: list<list<int>>] {
    let font_id = 3 + 2 * ($sizes | length)
    let kids = $sizes | enumerate | each {|page| $"(3 + 2 * $page.index) 0 R" } | str join " "
    mut objects = ["<< /Type /Catalog /Pages 2 0 R >>" $"<< /Type /Pages /Count ($sizes | length) /Kids [($kids)] >>"]
    for page in ($sizes | enumerate) {
        let content_id = 4 + 2 * $page.index
        let width = $page.item.0
        let height = $page.item.1
        let label = "PAGE " + ($page.index + 1 | into string) + " - TOP LEFT"
        let content = $"q 0.1 0.4 0.7 rg 12 12 30 50 re f Q\nBT /F1 18 Tf 24 ($height - 36) Td " + "(" + $label + ") Tj ET\n" + $"BT /F1 12 Tf ($width - 150) 24 Td " + "(BOTTOM RIGHT) Tj ET\n"
        $objects = $objects | append $"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 ($width) ($height)] /Resources << /Font << /F1 ($font_id) 0 R >> >> /Contents ($content_id) 0 R >>"
        $objects = $objects | append $"<< /Length ($content | str length) >>\nstream\n($content)endstream"
    }
    $objects = $objects | append "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>"
    encode-pdf $objects
}

def encode-pdf [objects: list<string>] {
    mut text = "%PDF-1.7\n"
    mut offsets = []
    for object in ($objects | enumerate) {
        $offsets = $offsets | append ($text | str length)
        $text = $text + $"($object.index + 1) 0 obj\n($object.item)\nendobj\n"
    }
    let start = $text | str length
    let xref = $offsets | each {|offset| ($offset | into string | fill --alignment right --character '0' --width 10) + " 00000 n \n" } | str join ""
    $text + $"xref\n0 (($objects | length) + 1)\n0000000000 65535 f \n($xref)trailer\n<< /Size (($objects | length) + 1) /Root 1 0 R >>\nstartxref\n($start)\n%%EOF\n"
}

# No BleedBox: the red media band becomes artwork only with a manual override.
# Media is 6x8 inches; the blue CropBox/TrimBox is 4x6, offset one inch per side.
def manual-bleed-pdf [] {
    let content = "1 0 0 rg 0 0 432 576 re f 0 0 1 rg 72 72 288 432 re f 1 1 1 rg BT /F1 12 Tf 90 480 Td (FINISHED 4 X 6) Tj ET\n"
    encode-pdf [
        "<< /Type /Catalog /Pages 2 0 R >>"
        "<< /Type /Pages /Count 1 /Kids [3 0 R] >>"
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 432 576] /CropBox [72 72 360 504] /TrimBox [72 72 360 504] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
        $"<< /Length ($content | str length) >>\nstream\n($content)endstream"
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>"
    ]
}

def main [directory: path] {
    mkdir $directory
    pdf [[612 792] [612 792] [612 792] [612 792] [612 792]] | save --force ($directory | path join source.pdf)
    pdf [[360 360] [360 360]] | save --force ($directory | path join replacement.pdf)
    pdf [[612 792] [595 842] [792 612]] | save --force ($directory | path join mixed.pdf)
    manual-bleed-pdf | save --force ($directory | path join manual-bleed.pdf)
    "%PDF-not-readable" | save --force ($directory | path join corrupt.pdf)
}
