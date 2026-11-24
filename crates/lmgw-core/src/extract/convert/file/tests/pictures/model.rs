//! Picture export with the layout model's regions (WP10 on WP8): real
//! poppler, the model's answer recorded (`layout_model::RECORDED`). A
//! picture region over a raster image is one figure with poppler's box, a
//! region with no image (a drawing) gets its picture from the render, an
//! image the model did not see keeps its figure, a caption the model marked
//! is the figure's once, and the text inside a picture stays out of the
//! markdown in every `pictures` mode while json keeps it.

use lmgw_api_types::documents::convert::Pictures;
use serde_json::Value;

use super::*;
use crate::extract::convert::layout_model::{Label, PageRegions, Region, Regions, RECORDED};

/// One region on an A4 page as displayed, in points.
fn reg(label: Label, (w, h): (f32, f32), x0: f32, y0: f32, x1: f32, y1: f32) -> Region {
    Region::checked(label, 0.9, x0 / w, y0 / h, x1 / w, y1 / h).unwrap()
}

const A4: (f32, f32) = (595.0, 842.0);

/// Three pages: a chart image under a picture region with text inside it
/// and a marked caption; a drawing (no image) under a picture region with
/// its marked caption; a photo the model did not mark, its caption read as
/// text.
fn paper() -> (Vec<u8>, Regions) {
    let pdf = build::pdf(&[
        Page::new(
            vec![
                (72.0, 80.0, 20.0, "Example Report"),
                (72.0, 120.0, 11.0, "The example text before the chart."),
                (100.0, 200.0, 9.0, "Example axis label"),
                (72.0, 350.0, 11.0, "Figure 1: Example chart"),
                (72.0, 380.0, 11.0, "The example text after the chart."),
            ],
            vec![Drawn::at(
                72.0,
                140.0,
                300.0,
                200.0,
                (30, 20),
                [10, 60, 200],
            )],
        ),
        Page::new(
            vec![
                (72.0, 120.0, 11.0, "The second page starts here."),
                (100.0, 220.0, 9.0, "Example drawing label"),
                (72.0, 360.0, 11.0, "Figure 2: Example drawing"),
            ],
            Vec::new(),
        ),
        Page::new(
            vec![
                (72.0, 120.0, 11.0, "The third page starts here."),
                (72.0, 360.0, 11.0, "Figure 3: Example photo"),
            ],
            vec![Drawn::at(
                72.0,
                140.0,
                200.0,
                200.0,
                (20, 20),
                [20, 160, 40],
            )],
        ),
    ]);
    let mut r = Regions::default();
    r.pages.insert(
        1,
        PageRegions::Found(vec![
            reg(Label::Title, A4, 66.0, 74.0, 300.0, 104.0),
            reg(Label::Text, A4, 66.0, 114.0, 530.0, 136.0),
            reg(Label::Picture, A4, 66.0, 137.0, 378.0, 343.0),
            reg(Label::Caption, A4, 66.0, 345.0, 330.0, 368.0),
            reg(Label::Text, A4, 66.0, 375.0, 530.0, 398.0),
        ]),
    );
    r.pages.insert(
        2,
        PageRegions::Found(vec![
            reg(Label::Text, A4, 66.0, 114.0, 530.0, 136.0),
            reg(Label::Picture, A4, 66.0, 150.0, 378.0, 345.0),
            reg(Label::Caption, A4, 66.0, 355.0, 330.0, 378.0),
        ]),
    );
    r.pages.insert(
        3,
        PageRegions::Found(vec![
            reg(Label::Text, A4, 66.0, 114.0, 530.0, 136.0),
            reg(Label::Text, A4, 66.0, 355.0, 330.0, 378.0),
        ]),
    );
    (pdf, r)
}

fn plan(pictures: Pictures, output: Output) -> Plan {
    plan_of(ConvertOptions {
        pictures,
        output,
        ..Default::default()
    })
}

async fn with_regions(pdf: Vec<u8>, r: Regions, plan: &Plan, facts: &ToolFacts) -> Value {
    let d = RECORDED
        .scope(r, convert(input(pdf), plan, facts))
        .await
        .doc;
    serde_json::to_value(&d).unwrap()
}

fn has_warning(d: &Value, code: &str) -> bool {
    d["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|w| w["code"] == code)
}

#[tokio::test]
async fn model_regions_and_poppler_s_pictures_are_one_figure_each_with_one_caption() {
    let Some(facts) = with_poppler().await else {
        return;
    };
    let (pdf, regions) = paper();
    for (mode, line) in [
        (Pictures::Placeholder, "<!-- image -->"),
        (Pictures::Embedded, "![Image](data:image/png;base64,"),
        (Pictures::Referenced, "![Image](artifacts/image_"),
    ] {
        let p = plan(mode, Output::Markdown);
        let d = with_regions(pdf.clone(), regions.clone(), &p, &facts).await;
        assert_eq!(d["status"], "converted", "{mode:?} {d}");
        let md = d["content"].as_str().unwrap();
        assert_eq!(md.matches(line).count(), 3, "{mode:?} {md}");
        // The text inside the chart and the drawing is not in the markdown.
        assert!(!md.contains("Example axis label"), "{mode:?} {md}");
        assert!(!md.contains("Example drawing label"), "{mode:?} {md}");
        assert!(has_warning(&d, "picture_text"), "{mode:?} {d}");
        for caption in [
            "Figure 1: Example chart",
            "Figure 2: Example drawing",
            "Figure 3: Example photo",
        ] {
            assert_eq!(md.matches(caption).count(), 1, "{mode:?} {md}");
        }
        // Each picture before its caption, the text after the chart a
        // paragraph of its own after both.
        let at = |s: &str| md.find(s).unwrap();
        assert!(at(line) < at("Figure 1") && at("Figure 1") < at("text after the chart"));
        // No caption without its picture is said.
        assert!(
            !d["warnings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|w| w["code"] == "pictures_partial"
                    && w["message"].as_str().unwrap().contains("figure caption")),
            "{d}"
        );
        let pics = d["pictures"].as_array().unwrap();
        assert_eq!(pics.len(), 3, "{mode:?} {d}");
        let captions: Vec<&str> = pics
            .iter()
            .map(|p| p["caption"].as_str().unwrap())
            .collect();
        assert_eq!(
            captions,
            [
                "Figure 1: Example chart",
                "Figure 2: Example drawing",
                "Figure 3: Example photo"
            ]
        );
        // Poppler's box for the chart, the region's for the drawing.
        assert_eq!(
            pics[0]["bbox"],
            serde_json::json!([72.0, 140.0, 372.0, 340.0])
        );
        let b = pics[1]["bbox"].as_array().unwrap();
        assert!((b[0].as_f64().unwrap() - 66.0).abs() < 0.2, "{b:?}");
        assert!((b[3].as_f64().unwrap() - 345.0).abs() < 0.2, "{b:?}");
        if mode == Pictures::Referenced {
            for p in pics {
                assert!(p["data_base64"].is_string() && p["file"].is_string(), "{p}");
            }
            // The drawing, cut from the render at 144 dpi: 312 × 195 pt.
            let w = pics[1]["width_px"].as_u64().unwrap();
            let h = pics[1]["height_px"].as_u64().unwrap();
            assert!(w.abs_diff(624) <= 2 && h.abs_diff(390) <= 2, "{w} × {h}");
        }
    }

    let d = with_regions(
        pdf,
        regions,
        &plan(Pictures::Referenced, Output::Json),
        &facts,
    )
    .await;
    let blocks: Vec<&Value> = d["structure"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|p| p["blocks"].as_array().unwrap())
        .collect();
    let figures: Vec<&&Value> = blocks.iter().filter(|b| b["kind"] == "figure").collect();
    assert_eq!(figures.len(), 3, "{blocks:?}");
    assert_eq!(figures[0]["source"], "onnx");
    assert_eq!(figures[0]["text"], "Example axis label");
    assert_eq!(figures[0]["picture"], "p1");
    assert_eq!(figures[0]["caption"], "Figure 1: Example chart");
    assert_eq!(
        figures[0]["bbox"],
        serde_json::json!([72.0, 140.0, 372.0, 340.0])
    );
    assert_eq!(figures[1]["source"], "onnx");
    assert_eq!(figures[1]["text"], "Example drawing label");
    assert_eq!(figures[1]["picture"], "p2");
    assert_eq!(figures[2]["source"], "text_layer");
    assert!(figures[2]["text"].is_null());
    assert_eq!(figures[2]["caption"], "Figure 3: Example photo");
    // Each caption once, as a caption block.
    let captions = blocks.iter().filter(|b| b["kind"] == "caption").count();
    assert_eq!(captions, 3, "{blocks:?}");
    assert!(blocks
        .iter()
        .any(|b| b["kind"] == "paragraph" && b["text"] == "The example text after the chart."));
}

#[tokio::test]
async fn turned_pages_place_regions_and_pictures_in_one_space() {
    let Some(facts) = with_poppler().await else {
        return;
    };
    for rotate in [90, 270] {
        let mut page = Page::new(
            vec![
                (72.0, 80.0, 12.0, "Example text"),
                (72.0, 110.0, 12.0, "More example text"),
            ],
            vec![Drawn::at(100.0, 300.0, 300.0, 200.0, (6, 4), [220, 20, 20])],
        );
        page.rotate = rotate;
        let pdf = build::pdf(&[page]);
        let json = plan(Pictures::Referenced, Output::Json);
        // Without the model: where poppler puts the picture and the lines.
        let d = serde_json::to_value(convert(input(pdf.clone()), &json, &facts).await.doc).unwrap();
        let part = &d["structure"][0];
        assert_eq!(
            (part["width_pt"].as_f64(), part["height_pt"].as_f64()),
            (Some(842.0), Some(595.0))
        );
        let bbox = |v: &Value| -> [f32; 4] {
            let a = v.as_array().unwrap();
            std::array::from_fn(|i| a[i].as_f64().unwrap() as f32)
        };
        let shown = (842.0, 595.0);
        let mut found = Vec::new();
        for b in part["blocks"].as_array().unwrap() {
            let [x0, y0, x1, y1] = bbox(&b["bbox"]);
            let label = if b["kind"] == "figure" {
                Label::Picture
            } else {
                Label::Text
            };
            found.push(reg(label, shown, x0 - 3.0, y0 - 2.0, x1 + 3.0, y1 + 2.0));
        }
        let pic = bbox(&d["pictures"][0]["bbox"]);
        let mut r = Regions::default();
        r.pages.insert(1, PageRegions::Found(found));
        let m = with_regions(pdf, r, &json, &facts).await;
        assert!(!has_warning(&m, "part_failed"), "rotate {rotate}: {m}");
        let blocks = m["structure"][0]["blocks"].as_array().unwrap();
        let figures: Vec<&Value> = blocks.iter().filter(|b| b["kind"] == "figure").collect();
        assert_eq!(figures.len(), 1, "rotate {rotate}: {blocks:?}");
        assert_eq!(figures[0]["source"], "onnx");
        assert_eq!(bbox(&figures[0]["bbox"]), pic, "rotate {rotate}");
        assert_eq!(m["pictures"].as_array().unwrap().len(), 1);
        assert!(m["pictures"][0]["data_base64"].is_string());
    }
}
