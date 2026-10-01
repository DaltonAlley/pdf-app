use super::*;

fn labeled_mixed_pdf() -> Vec<u8> {
    let mut doc = LoDocument::with_version("1.7");
    let pages_id = doc.new_object_id();
    let mut ids = Vec::new();
    let font =
        doc.add_object(dictionary! {"Type"=>"Font","Subtype"=>"Type1","BaseFont"=>"Helvetica"});
    for (label, w) in [("FRONT", 576), ("BACK", 720)] {
        let half = w / 2;
        let content=format!("1 0 0 rg 0 144 {half} 144 re f 0 1 0 rg {half} 144 {half} 144 re f 0 0 1 rg 0 0 {half} 144 re f 1 1 0 rg {half} 0 {half} 144 re f 0 0 0 rg BT /F1 12 Tf 12 138 Td ({label} LEFT) Tj ET BT /F1 12 Tf {} 138 Td ({label} RIGHT) Tj ET",half+12);
        let contents = doc.add_object(Stream::new(Dictionary::new(), content.into_bytes()));
        ids.push(doc.add_object(dictionary!{"Type"=>"Page","Parent"=>pages_id,"MediaBox"=>vec![0.into(),0.into(),w.into(),288.into()],"Contents"=>contents,"Resources"=>dictionary!{"Font"=>dictionary!{"F1"=>font}}}));
    }
    doc.objects.insert(pages_id,Object::Dictionary(dictionary!{"Type"=>"Pages","Kids"=>ids.into_iter().map(Object::Reference).collect::<Vec<_>>(),"Count"=>2}));
    let root = doc.add_object(dictionary! {"Type"=>"Catalog","Pages"=>pages_id});
    doc.trailer.set("Root", root);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    bytes
}
async fn prepare(state: Arc<AppState>, bytes: Vec<u8>) -> serde_json::Value {
    let response = send(
        state.clone(),
        multipart_request(
            "/gang-up/sources",
            vec![upload_part("files", "labeled.pdf", bytes)],
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let id = body_text(response).await;
    wait_for_job_status(state.clone(), &id, "done").await;
    let download = send(state, get_request(&format!("/jobs/{id}/download"))).await;
    serde_json::from_slice(&body_bytes(download).await).unwrap()
}
fn color(image: &image::DynamicImage, x: f64, y: f64, expected: [u8; 3]) {
    let pixel = image
        .get_pixel((x * 72.0).round() as u32, (y * 72.0).round() as u32)
        .0;
    assert!(
        pixel[..3]
            .iter()
            .zip(expected)
            .all(|(a, b)| a.abs_diff(b) < 8),
        "pixel at {x},{y}: {pixel:?}, expected {expected:?}"
    );
}

#[tokio::test]
#[serial(pdfium)]
async fn finished_size_source_binding_and_labeled_asymmetric_duplex_pixels_match_plans() {
    let Some(state) = state_or_skip() else {
        return;
    };
    let source = labeled_mixed_pdf();
    let prepared = prepare(state.clone(), source.clone()).await;
    assert_eq!(
        prepared["analysis"]["sourcePages"][0]["sourcePdfSize"]["width"],
        8.0
    );
    assert_eq!(
        prepared["analysis"]["sourcePages"][1]["sourcePdfSize"]["width"],
        10.0
    );
    for edge in ["longEdge", "shortEdge"] {
        for rotate in [false, true] {
            let request = serde_json::json!({
                "sourceId":prepared["sourceId"],"sourcePdfSize":{"width":99.0,"height":99.0},"sourcePageCount":1,
                "sourcePages":[{"sourcePdfSize":{"width":99.0,"height":99.0}}],
                "finishedCutSize":{"width":4.0,"height":4.0},"parentSheetSize":{"width":12.0,"height":18.0},
                "quantityRequested":1,"impositionMode":"unique","sides":"double","orientationPreference":"upright",
                "duplex":{"flipEdge":edge,"rotateBack180":rotate},"layoutMode":"manual",
                "manual":{"rows":1,"columns":1,"rotationDegrees":0,"margins":{"left":1.0,"top":1.0,"right":6.75,"bottom":12.75}},
                "gutter":{"horizontal":0.0,"vertical":0.0},"bleedOption":"scaleToBleed","createdBleedAmount":0.125,
                "artworkFit":{"mode":"cover","position":{"x":0.0,"y":0.5}},
                "pageOverrides":[{"pageNumber":2,"artworkFit":{"mode":"cover","position":{"x":1.0,"y":0.5}}}]
            });
            let response = send(
                state.clone(),
                json_request("/gang-up/layout", request.clone()),
            )
            .await;
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "{}",
                body_text(response).await
            );
            let response = send(
                state.clone(),
                json_request("/gang-up/layout", request.clone()),
            )
            .await;
            let result: serde_json::Value =
                serde_json::from_slice(&body_bytes(response).await).unwrap();
            assert_eq!(result["sourcePdfSize"]["width"], 8.0);
            assert_eq!(result["sourcePageCount"], 2);
            assert_eq!(result["pagePlans"][1]["sourcePdfSize"]["width"], 10.0);
            let exported = send(
                state.clone(),
                multipart_request(
                    "/gang-up/export",
                    vec![
                        text_part("layoutRequest", &request.to_string()),
                        upload_part("file", "labeled.pdf", source.clone()),
                    ],
                ),
            )
            .await;
            assert_eq!(exported.status(), StatusCode::OK);
            let pdfium = state.pdfium();
            let doc = pdfium
                .load_pdf_from_byte_vec(body_bytes(exported).await, None)
                .unwrap();
            assert_eq!(doc.pages().len(), 2);
            let render = |i| {
                doc.pages()
                    .get(i)
                    .unwrap()
                    .render_with_config(&PdfRenderConfig::new().scale_page_by_factor(1.0))
                    .unwrap()
                    .as_image()
                    .unwrap()
            };
            let front = render(0);
            let back = render(1);
            color(&front, 2.0, 2.0, [255, 0, 0]);
            color(&front, 2.0, 4.0, [0, 0, 255]);
            let mirror_x = (edge == "longEdge") != rotate;
            let mirror_y = (edge == "longEdge") == rotate;
            let bx = if mirror_x { 6.75 } else { 1.0 };
            let by = if mirror_y { 12.75 } else { 1.0 };
            color(
                &back,
                bx + 1.0,
                by + 1.0,
                if rotate { [255, 255, 0] } else { [0, 255, 0] },
            );
            color(
                &back,
                bx + 1.0,
                by + 3.0,
                if rotate { [0, 255, 0] } else { [255, 255, 0] },
            );
            // Cover + created bleed fills the whole clip even at opposite edge anchors.
            color(&front, 1.04, 1.04, [255, 0, 0]);
            color(
                &back,
                bx + 0.04,
                by + 0.04,
                if rotate { [255, 255, 0] } else { [0, 255, 0] },
            );
        }
    }
}

#[tokio::test]
#[serial(pdfium)]
async fn supplied_bleed_outside_crop_is_visible_in_preview_and_export() {
    let Some(state) = state_or_skip() else {
        return;
    };
    let mut source =
        LoDocument::load_mem(&sample_business_card_with_bleed_boxes_pdf_bytes()).unwrap();
    let page_id = *source.get_pages().values().next().unwrap();
    let content_id = source.add_object(Stream::new(
        Dictionary::new(),
        b"1 0 0 rg 0 0 162 270 re f 0 0 1 rg 9 9 144 252 re f".to_vec(),
    ));
    source
        .get_dictionary_mut(page_id)
        .unwrap()
        .set("Contents", content_id);
    let mut bytes = Vec::new();
    source.save_to(&mut bytes).unwrap();
    let prepared = prepare(state.clone(), bytes.clone()).await;
    let page = &prepared["analysis"]["sourcePages"][0];
    assert_eq!(page["previewBox"]["width"], 2.25);
    assert_eq!(page["previewBox"]["height"], 3.75);
    let source_id = prepared["sourceId"].as_str().unwrap();
    let response = send(
        state.clone(),
        get_request(&format!("/gang-up/sources/{source_id}/preview/1")),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let preview = image::load_from_memory(&body_bytes(response).await).unwrap();
    assert!((preview.width() as f64 / preview.height() as f64 - 0.6).abs() < 0.005);
    let edge = preview.get_pixel(4, preview.height() / 2).0;
    assert!(
        edge[0] > 245 && edge[2] < 10,
        "outside-crop red bleed omitted: {edge:?}"
    );
    let center = preview
        .get_pixel(preview.width() / 2, preview.height() / 2)
        .0;
    assert!(center[2] > 245 && center[0] < 10);
    let request = serde_json::json!({"sourceId":source_id,"sourcePdfSize":{"width":2.25,"height":3.75},"sourcePageCount":1,
        "finishedCutSize":{"width":2.0,"height":3.5},"parentSheetSize":{"width":4.0,"height":6.0},
        "quantityRequested":1,"impositionMode":"unique","sides":"single","orientationPreference":"upright",
        "layoutMode":"manual","manual":{"rows":1,"columns":1,"rotationDegrees":0,"margins":null},
        "gutter":{"horizontal":0.0,"vertical":0.0},"bleedOption":"useAsIs","artworkFit":{"mode":"contain"}});
    let response = send(
        state.clone(),
        multipart_request(
            "/gang-up/export",
            vec![
                text_part("layoutRequest", &request.to_string()),
                upload_part("file", "bleed.pdf", bytes),
            ],
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let pdfium = state.pdfium();
    let doc = pdfium
        .load_pdf_from_byte_vec(body_bytes(response).await, None)
        .unwrap();
    let image = doc
        .pages()
        .get(0)
        .unwrap()
        .render_with_config(&PdfRenderConfig::new().scale_page_by_factor(1.0))
        .unwrap()
        .as_image()
        .unwrap();
    color(&image, 0.92, 3.0, [255, 0, 0]);
    color(&image, 2.0, 3.0, [0, 0, 255]);
}

#[tokio::test]
#[serial(pdfium)]
async fn finished_size_presets_and_resized_manual_bleed_history_roundtrip_without_source_binding() {
    let Some(state) = state_or_skip() else {
        return;
    };
    let preset = serde_json::json!({"name":"Letter cover with supplied bleed","finishedSizeMode":"common",
        "finishedCutSize":{"width":8.5,"height":11.0},"parentSheetSize":{"width":12.0,"height":18.0},
        "artworkFit":{"mode":"cover","position":{"x":0.2,"y":0.8}},"sourceBleedOverride":0.125,
        "bleedHandling":"useAsIs","gutter":{"horizontal":0.1,"vertical":0.1},"orientationPreference":"upright",
        "sides":"single","layoutPreference":"auto","sourceId":"must-not-be-saved","sourcePages":[],"pageOverrides":[]});
    let response = send(state.clone(), json_request("/gang-up/presets", preset)).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let saved: serde_json::Value = serde_json::from_slice(&body_bytes(response).await).unwrap();
    assert_eq!(saved["finishedSizeMode"], "common");
    assert_eq!(saved["artworkFit"]["mode"], "cover");
    assert_eq!(saved["artworkFit"]["position"]["x"], 0.2);
    assert_eq!(saved["sourceBleedOverride"], 0.125);
    assert!(saved.get("sourceId").is_none());
    assert!(saved.get("sourcePages").is_none());
    assert!(saved.get("pageOverrides").is_none());
    let listed = send(state.clone(), get_request("/gang-up/presets")).await;
    let listed: serde_json::Value = serde_json::from_slice(&body_bytes(listed).await).unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 2);
    assert!(listed.as_array().unwrap().contains(&saved));

    let pdfium = state.pdfium();
    let source = sample_pdf_bytes_with_size(&pdfium, 1, 4.25, 6.25);
    let prepared = prepare(state.clone(), source.clone()).await;
    let request = serde_json::json!({"sourceId":prepared["sourceId"],"sourcePdfSize":{"width":4.25,"height":6.25},"sourcePageCount":1,
        "finishedCutSize":{"width":8.5,"height":11.0},"parentSheetSize":{"width":12.0,"height":18.0},
        "quantityRequested":1,"impositionMode":"unique","sides":"single","layoutMode":"auto","manual":null,
        "gutter":{"horizontal":0.1,"vertical":0.1},"sourceBleedOverride":0.125,"bleedOption":"useAsIs",
        "artworkFit":{"mode":"cover","position":{"x":0.2,"y":0.8}}});
    let response=send(state.clone(),json_request("/gang-up/recent-jobs",serde_json::json!({"name":"Resize supplied bleed","request":request,"layoutSummary":null}))).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let recent: serde_json::Value = serde_json::from_slice(&body_bytes(response).await).unwrap();
    assert!(recent["request"]["sourceId"].is_null());
    assert_eq!(recent["request"]["finishedCutSize"]["width"], 8.5);
    let exported = send(
        state.clone(),
        multipart_request(
            "/gang-up/export",
            vec![
                text_part("layoutRequest", &request.to_string()),
                upload_part("file", "manual-bleed.pdf", source),
            ],
        ),
    )
    .await;
    assert_eq!(exported.status(), StatusCode::OK);
    assert!(
        !exported
            .headers()
            .contains_key("x-pdf-tools-export-warning"),
        "history persistence unexpectedly failed"
    );
    let records = send(state.clone(), get_request("/gang-up/export-history")).await;
    assert_eq!(records.status(), StatusCode::OK);
    let records: serde_json::Value = serde_json::from_slice(&body_bytes(records).await).unwrap();
    assert_eq!(records.as_array().unwrap().len(), 1);
    assert!(records[0]["request"]["sourceId"].is_null());
    assert_eq!(records[0]["request"]["sourceBleedOverride"], 0.125);
}

#[tokio::test]
#[serial(pdfium)]
async fn manual_bleed_expands_preview_to_actual_media_artwork_without_changing_default_crop() {
    let Some(state) = state_or_skip() else {
        return;
    };
    let mut source =
        LoDocument::load_mem(&sample_business_card_with_bleed_boxes_pdf_bytes()).unwrap();
    let id = *source.get_pages().values().next().unwrap();
    let content = source.add_object(Stream::new(
        Dictionary::new(),
        b"1 0 0 rg 0 0 432 576 re f 0 0 1 rg 72 72 288 432 re f".to_vec(),
    ));
    let page = source.get_dictionary_mut(id).unwrap();
    page.set("MediaBox", vec![0.into(), 0.into(), 432.into(), 576.into()]);
    page.set(
        "CropBox",
        vec![72.into(), 72.into(), 360.into(), 504.into()],
    );
    page.set(
        "TrimBox",
        vec![72.into(), 72.into(), 360.into(), 504.into()],
    );
    page.remove(b"BleedBox");
    page.set("Contents", content);
    let mut bytes = Vec::new();
    source.save_to(&mut bytes).unwrap();
    let prepared = prepare(state.clone(), bytes.clone()).await;
    assert_eq!(
        prepared["analysis"]["sourcePdfSize"],
        serde_json::json!({"width":4.0,"height":6.0})
    );
    let source_id = prepared["sourceId"].as_str().unwrap();
    let url = format!("/gang-up/sources/{source_id}/preview/1");
    let default = send(state.clone(), get_request(&url)).await;
    assert_eq!(default.status(), StatusCode::OK);
    let default = image::load_from_memory(&body_bytes(default).await).unwrap();
    let pixel = default.get_pixel(4, default.height() / 2).0;
    assert!(
        pixel[2] > 245 && pixel[0] < 10,
        "default CropBox changed: {pixel:?}"
    );
    let manual = send(
        state.clone(),
        get_request(&format!("{url}?sourceBleedOverride=0.125")),
    )
    .await;
    assert_eq!(manual.status(), StatusCode::OK);
    let manual = image::load_from_memory(&body_bytes(manual).await).unwrap();
    let pixel = manual.get_pixel(4, manual.height() / 2).0;
    assert!(
        pixel[0] > 245 && pixel[2] < 10,
        "manual bleed red band missing: {pixel:?}"
    );
    assert!((manual.width() as f64 / manual.height() as f64 - 4.25 / 6.25).abs() < 0.002);
    let batch = send(
        state.clone(),
        json_request(
            &format!("/gang-up/sources/{source_id}/previews"),
            serde_json::json!({"pageNumbers":[1],"sourceBleedOverride":0.125}),
        ),
    )
    .await;
    assert_eq!(batch.status(), StatusCode::OK);
    let batch = preview_batch_pages(&body_bytes(batch).await);
    let batch = image::load_from_memory(&batch[&1]).unwrap();
    assert_eq!(batch.to_rgba8(), manual.to_rgba8());
    for amount in ["-0.1", "0", "1.1", "NaN"] {
        let response = send(
            state.clone(),
            get_request(&format!("{url}?sourceBleedOverride={amount}")),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "accepted invalid preview bleed {amount}"
        );
    }
    let request = serde_json::json!({"sourceId":source_id,"sourcePdfSize":{"width":4.0,"height":6.0},"sourcePageCount":1,
        "finishedCutSize":{"width":4.0,"height":6.0},"parentSheetSize":{"width":6.0,"height":8.0},
        "quantityRequested":1,"impositionMode":"unique","sides":"single","orientationPreference":"upright",
        "layoutMode":"manual","manual":{"rows":1,"columns":1,"rotationDegrees":0,"margins":null},
        "sourceBleedOverride":0.125,"gutter":{"horizontal":0.0,"vertical":0.0},"bleedOption":"useAsIs","artworkFit":{"mode":"contain"}});
    let layout = send(
        state.clone(),
        json_request("/gang-up/layout", request.clone()),
    )
    .await;
    assert_eq!(layout.status(), StatusCode::OK);
    let layout: serde_json::Value = serde_json::from_slice(&body_bytes(layout).await).unwrap();
    assert_eq!(layout["sourceBleedOverride"], 0.125);
    assert_eq!(
        layout["pagePlans"][0]["sourcePdfSize"],
        serde_json::json!({"width":4.25,"height":6.25})
    );
    assert_eq!(layout["pagePlans"][0]["previewBox"]["left"], 0.0);
    assert_eq!(layout["pagePlans"][0]["previewBox"]["width"], 4.25);
    let exported = send(
        state.clone(),
        multipart_request(
            "/gang-up/export",
            vec![
                text_part("layoutRequest", &request.to_string()),
                upload_part("file", "manual-media.pdf", bytes),
            ],
        ),
    )
    .await;
    assert_eq!(exported.status(), StatusCode::OK);
    let pdfium = state.pdfium();
    let doc = pdfium
        .load_pdf_from_byte_vec(body_bytes(exported).await, None)
        .unwrap();
    let image = doc
        .pages()
        .get(0)
        .unwrap()
        .render_with_config(&PdfRenderConfig::new().scale_page_by_factor(1.0))
        .unwrap()
        .as_image()
        .unwrap();
    color(&image, 0.92, 4.0, [255, 0, 0]);
    color(&image, 3.0, 4.0, [0, 0, 255]);
    // Per-piece clipping still excludes the rest of the six-inch-wide media.
    color(&image, 0.5, 4.0, [255, 255, 255]);
    let unchanged = send(state.clone(), get_request(&url)).await;
    let unchanged = image::load_from_memory(&body_bytes(unchanged).await).unwrap();
    assert_eq!(unchanged.to_rgba8(), default.to_rgba8());
}
