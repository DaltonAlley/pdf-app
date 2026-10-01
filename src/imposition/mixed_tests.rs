use super::*;

fn request() -> LayoutRequest {
    serde_json::from_value(serde_json::json!({
        "sourcePdfSize":{"width":20.0,"height":2.0}, "sourcePageCount":1,
        "finishedCutSize":{"width":8.5,"height":11.0},
        "parentSheetSize":{"width":12.0,"height":18.0},
        "quantityRequested":1,"impositionMode":"unique","orientationPreference":"upright",
        "sides":"single","layoutMode":"auto","bleedOption":"useAsIs",
        "gutter":{"horizontal":0.0,"vertical":0.0},"manual":null,
        "artworkFit":{"mode":"cover","position":{"x":0.5,"y":0.5}}
    }))
    .unwrap()
}
fn page(width: f64, height: f64) -> SourcePage {
    SourcePage {
        source_pdf_size: SizeInches { width, height },
        source_trim_box: None,
        preview_box: None,
        physical_size_assumed: false,
        filename: None,
        original_page_number: None,
    }
}
fn close(a: f64, b: f64) {
    assert!((a - b).abs() < 0.00001, "{a} != {b}");
}
fn layout(r: LayoutRequest) -> LayoutResult {
    super::super::layout::generate_layout(r).unwrap()
}

#[test]
fn odd_aspect_letter_cover_is_uniform_and_predictable() {
    let result = layout(request());
    let plan = &result.page_plans[0];
    close(plan.artwork.width, 110.0);
    close(plan.artwork.height, 11.0);
    close(plan.artwork.x, -50.75);
    close(plan.artwork.y, 0.0);
    close(plan.artwork.width / 20.0, plan.artwork.height / 2.0);
    assert_eq!(
        result.source_pdf_size,
        SizeInches {
            width: 20.0,
            height: 2.0
        }
    );
}
#[test]
fn odd_aspect_letter_contain_keeps_entire_source_without_stretch() {
    let mut r = request();
    r.artwork_fit.as_mut().unwrap().mode = ArtworkFitMode::Contain;
    let result = layout(r);
    let a = result.page_plans[0].artwork;
    close(a.width, 8.5);
    close(a.height, 0.85);
    close(a.x, 0.0);
    close(a.y, 5.075);
}
#[test]
fn cover_and_contain_respect_all_normalized_corner_anchors() {
    for mode in [ArtworkFitMode::Contain, ArtworkFitMode::Cover] {
        for x in [0.0, 0.5, 1.0] {
            for y in [0.0, 0.5, 1.0] {
                let mut r = request();
                r.artwork_fit = Some(ArtworkFit {
                    mode,
                    position: CropPosition { x, y },
                });
                let result = layout(r);
                let a = result.page_plans[0].artwork;
                close(a.x, (8.5 - a.width) * x);
                close(a.y, (11.0 - a.height) * y);
            }
        }
    }
}
#[test]
fn adding_bleed_keeps_anchor_and_source_aspect_separate_from_fit() {
    for mode in [ArtworkFitMode::Contain, ArtworkFitMode::Cover] {
        for x in [0.0, 0.5, 1.0] {
            let mut r = request();
            r.artwork_fit = Some(ArtworkFit {
                mode,
                position: CropPosition { x, y: 1.0 },
            });
            let fitted = layout(r.clone()).page_plans[0].artwork;
            r.bleed_option = BleedOption::ScaleToBleed;
            r.created_bleed_amount = 0.125;
            let result = layout(r);
            let p = &result.page_plans[0];
            let a = p.artwork;
            close(a.width / fitted.width, 8.75 / 8.5);
            close(a.width / 20.0, a.height / 2.0);
            close(a.x, -0.125 + (8.75 - a.width) * x);
            close(a.y, -0.125 + 11.25 - a.height);
            close(a.x - (8.5 - a.width) * x, 0.125 * (2.0 * x - 1.0));
            if mode == ArtworkFitMode::Cover {
                assert!(a.x <= -0.125 + 1e-9 && a.y <= -0.125 + 1e-9);
                assert!(a.x + a.width >= 8.625 - 1e-9 && a.y + a.height >= 11.125 - 1e-9);
            }
            close(p.bleed_amount, 0.125);
            assert!(result.placements[0].finished_width >= 8.75);
            assert!(!result.warnings.is_empty());
        }
    }
}
#[test]
fn mixed_original_sizes_keep_individual_cuts_in_largest_regular_slots() {
    let mut r = request();
    r.source_pages = vec![page(4.0, 6.0), page(6.0, 4.0)];
    r.source_page_count = Some(2);
    r.finished_size_mode = FinishedSizeMode::Original;
    r.parent_sheet_size = SizeInches {
        width: 12.0,
        height: 12.0,
    };
    let result = layout(r);
    assert_eq!(result.pieces_per_sheet, 4);
    for p in &result.page_plans {
        close(p.artwork.width, p.source_pdf_size.width);
        close(p.artwork.height, p.source_pdf_size.height);
        let (_, clip) = placed_rects(p, &result.placements[0], 0);
        close(clip.x + clip.width / 2.0, 3.0);
        close(clip.y + clip.height / 2.0, 3.0);
    }
    assert_eq!(
        result.page_plans[0].finished_cut_size,
        SizeInches {
            width: 4.0,
            height: 6.0
        }
    );
    assert_eq!(
        result.page_plans[1].finished_cut_size,
        SizeInches {
            width: 6.0,
            height: 4.0
        }
    );
}
#[test]
fn per_page_overrides_do_not_mutate_sources_or_other_pages() {
    let mut r = request();
    r.source_pages = vec![page(4.0, 6.0), page(6.0, 4.0)];
    r.source_page_count = Some(2);
    r.page_overrides = vec![PageOverride {
        page_number: 2,
        finished_cut_size: Some(SizeInches {
            width: 3.0,
            height: 5.0,
        }),
        artwork_fit: Some(ArtworkFit {
            mode: ArtworkFitMode::Contain,
            position: CropPosition { x: 0.0, y: 0.0 },
        }),
    }];
    let result = layout(r);
    let a = &result.page_plans[0];
    let b = &result.page_plans[1];
    assert_eq!(
        a.finished_cut_size,
        SizeInches {
            width: 8.5,
            height: 11.0
        }
    );
    assert_eq!(
        b.source_pdf_size,
        SizeInches {
            width: 6.0,
            height: 4.0
        }
    );
    close(b.artwork.width, 3.0);
    close(b.artwork.height, 2.0);
}
#[test]
fn crop_positions_and_overrides_are_bounded() {
    for x in [-0.01, 1.01, f64::NAN, f64::INFINITY] {
        let mut r = request();
        r.artwork_fit.as_mut().unwrap().position.x = x;
        assert!(generate(r)
            .unwrap_err()
            .to_string()
            .contains("crop position"));
    }
    for number in [0, 2, usize::MAX] {
        let mut r = request();
        r.page_overrides.push(PageOverride {
            page_number: number,
            finished_cut_size: None,
            artwork_fit: None,
        });
        assert!(generate(r)
            .unwrap_err()
            .to_string()
            .contains("existing pages"));
    }
}
#[test]
fn duplex_requires_even_pages_and_matching_individual_cuts() {
    let mut r = request();
    r.sides = Sides::Double;
    r.duplex = Some(DuplexSettings {
        flip_edge: DuplexFlipEdge::LongEdge,
        rotate_back_180: false,
        back_alignment: String::new(),
    });
    assert!(generate(r.clone())
        .unwrap_err()
        .to_string()
        .contains("even"));
    r.source_page_count = Some(2);
    r.source_pages = vec![page(4.0, 6.0), page(6.0, 4.0)];
    assert!(generate(r.clone()).is_ok());
    r.finished_size_mode = FinishedSizeMode::Original;
    assert!(generate(r.clone())
        .unwrap_err()
        .to_string()
        .contains("front and back finished sizes"));
    r.page_overrides.push(PageOverride {
        page_number: 2,
        finished_cut_size: Some(SizeInches {
            width: 4.0,
            height: 6.0,
        }),
        artwork_fit: None,
    });
    assert!(generate(r).is_ok());
}
#[test]
fn unknown_image_physical_size_requires_explicit_finished_size() {
    let mut r = request();
    let mut image = page(2.0, 1.0);
    image.physical_size_assumed = true;
    r.source_pages = vec![image];
    assert!(generate(r.clone()).is_ok());
    r.finished_size_mode = FinishedSizeMode::Original;
    assert!(generate(r.clone())
        .unwrap_err()
        .to_string()
        .contains("300 DPI"));
    r.page_overrides.push(PageOverride {
        page_number: 1,
        finished_cut_size: Some(SizeInches {
            width: 4.0,
            height: 6.0,
        }),
        artwork_fit: None,
    });
    assert!(generate(r).is_ok());
}
#[test]
fn quarter_turn_rotates_asymmetric_artwork_around_its_cut() {
    let mut r = request();
    r.orientation_preference = OrientationPreference::QuarterTurn;
    r.artwork_fit.as_mut().unwrap().position = CropPosition { x: 0.0, y: 1.0 };
    let result = layout(r);
    assert_eq!(result.rotation_degrees, 90);
    let p = &result.page_plans[0];
    let slot = &result.placements[0];
    let (a, c) = placed_rects(p, slot, 90);
    close(a.width, p.artwork.height);
    close(a.height, p.artwork.width);
    close(a.x - c.x, 11.0 - p.artwork.y - p.artwork.height);
    close(a.y - c.y, p.artwork.x);
}
#[test]
fn legacy_requests_default_to_unchanged_geometry_path() {
    let mut r = request();
    r.artwork_fit = None;
    assert!(!enabled(&r));
    assert!(layout(r).page_plans.is_empty());
}

#[test]
fn stretch_fits_each_axis_exactly_without_position_travel() {
    for source in [page(20.0, 2.0), page(2.0, 20.0)] {
        for option in [BleedOption::UseAsIs, BleedOption::FitInside] {
            for position in [CropPosition { x: 0.0, y: 1.0 }, CropPosition::default()] {
                let mut r = request();
                r.source_pages = vec![source.clone()];
                r.bleed_option = option;
                r.artwork_fit = Some(ArtworkFit {
                    mode: ArtworkFitMode::Stretch,
                    position,
                });
                let p = layout(r).page_plans.remove(0);
                close(p.artwork.x, 0.0);
                close(p.artwork.y, 0.0);
                close(p.artwork.width, 8.5);
                close(p.artwork.height, 11.0);
                close(p.position_travel.x, 0.0);
                close(p.position_travel.y, 0.0);
                close(p.bleed_amount, 0.0);
            }
        }
    }
}

#[test]
fn stretch_adds_bleed_after_fitting_using_uniform_enlargement() {
    for x in [0.0, 0.5, 1.0] {
        for y in [0.0, 0.5, 1.0] {
            let mut r = request();
            r.artwork_fit = Some(ArtworkFit {
                mode: ArtworkFitMode::Stretch,
                position: CropPosition { x, y },
            });
            r.bleed_option = BleedOption::ScaleToBleed;
            r.created_bleed_amount = 0.125;
            let p = layout(r).page_plans.remove(0);
            let enlargement = 8.75 / 8.5;
            close(p.artwork.width, 8.5 * enlargement);
            close(p.artwork.height, 11.0 * enlargement);
            close(p.artwork.x, -0.125);
            close(p.artwork.y, -0.125 + (11.25 - p.artwork.height) * y);
            close(p.bleed_amount, 0.125);
            assert!(p.artwork.y + p.artwork.height >= 11.125 - 1e-9);
        }
    }
}

#[test]
fn stretch_supplied_bleed_uses_smallest_transformed_edge_and_trim_offsets() {
    let mut r = request();
    r.source_pages = vec![SourcePage {
        source_trim_box: Some(PdfBox {
            left: 0.5,
            bottom: 0.25,
            right: 4.5,
            top: 2.25,
            width: 4.0,
            height: 2.0,
        }),
        ..page(5.0, 3.0)
    }];
    r.finished_cut_size = SizeInches {
        width: 8.0,
        height: 6.0,
    };
    r.artwork_fit.as_mut().unwrap().mode = ArtworkFitMode::Stretch;
    let p = layout(r).page_plans.remove(0);
    close(p.artwork.width, 10.0);
    close(p.artwork.height, 9.0);
    close(p.artwork.x, -1.0);
    close(p.artwork.y, -2.25);
    close(p.bleed_amount, 0.75);
    close(p.position_travel.x, 0.0);
    close(p.position_travel.y, 0.0);
}

#[test]
fn stretch_page_override_and_request_roundtrip_preserve_other_fit_modes() {
    let mut r = request();
    r.source_pages = vec![page(20.0, 2.0), page(2.0, 20.0)];
    r.source_page_count = Some(2);
    r.page_overrides.push(PageOverride {
        page_number: 2,
        finished_cut_size: Some(SizeInches {
            width: 3.0,
            height: 5.0,
        }),
        artwork_fit: Some(ArtworkFit {
            mode: ArtworkFitMode::Stretch,
            position: CropPosition::default(),
        }),
    });
    let serialized = serde_json::to_value(&r).unwrap();
    assert_eq!(
        serialized["pageOverrides"][0]["artworkFit"]["mode"],
        "stretch"
    );
    let restored: LayoutRequest = serde_json::from_value(serialized).unwrap();
    assert_eq!(restored, r);
    let result = layout(restored);
    close(result.page_plans[0].artwork.width, 110.0);
    close(result.page_plans[1].artwork.width, 3.0);
    close(result.page_plans[1].artwork.height, 5.0);
}
