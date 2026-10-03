#[cfg(test)]
mod tests {
    use crate::trace::{
        export_dir_from_project, next_stem_sequence, prepare_straight_keep_alpha, rasterize_svg,
        resize_if_needed, resolve_export_filename, sanitize_stem, stem_from_image_path,
        to_binary_bw, unpremultiply_rgba, vectorize_file, vectorize_rgba, ExportMeta, TraceConfig,
        MAX_SIDE,
    };
    use image::{Rgba, RgbaImage};
    use std::path::PathBuf;

    #[test]
    fn binary_trace_smoke() {
        let mut img = RgbaImage::from_pixel(64, 64, Rgba([255, 255, 255, 255]));
        for y in 8..56 {
            for x in 8..56 {
                img.put_pixel(x, y, Rgba([0, 0, 0, 255]));
            }
        }
        for y in 20..44 {
            for x in 20..44 {
                img.put_pixel(x, y, Rgba([255, 255, 255, 255]));
            }
        }
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("smoke_ring.png");
        img.save(&path).expect("save png");
        let cfg = TraceConfig {
            mode: 0,
            ..TraceConfig::default()
        };
        let result = vectorize_file(&path, &cfg).expect("vectorize");
        let (buf, w, h) = rasterize_svg(&result.svg).expect("raster");
        let mut blackish = 0u32;
        for px in buf.chunks(4) {
            if px[3] >= 8 && px[0] < 40 && px[1] < 40 && px[2] < 40 {
                blackish += 1;
            }
        }
        eprintln!("binary ring {w}x{h} blackish={blackish}\n{}", result.svg);
        assert!(blackish > 100, "expected visible black ink, got {blackish}");
        assert!(
            blackish < 3500,
            "ring should not fill entire canvas, blackish={blackish}"
        );
    }

    #[test]
    fn binary_solid_black_visible() {
        let mut img = RgbaImage::from_pixel(64, 64, Rgba([255, 255, 255, 255]));
        for y in 8..56 {
            for x in 8..56 {
                img.put_pixel(x, y, Rgba([0, 0, 0, 255]));
            }
        }
        let cfg = TraceConfig {
            mode: 0,
            ..TraceConfig::default()
        };
        let r = vectorize_rgba(img.as_raw(), 64, 64, &cfg).expect("vectorize");
        let (buf, _, _) = rasterize_svg(&r.svg).expect("raster");
        let mut blackish = 0u32;
        for px in buf.chunks(4) {
            if px[3] >= 8 && px[0] < 40 {
                blackish += 1;
            }
        }
        eprintln!("solid blackish={blackish}\n{}", r.svg);
        assert!(blackish > 500, "solid black should remain visible");
        assert!(
            blackish < 4000,
            "solid should not paint entire canvas including former white bg"
        );
    }

    #[test]
    fn color_trace_keeps_hues() {
        let mut img = RgbaImage::from_pixel(64, 64, Rgba([255, 255, 255, 0]));
        for y in 8..56 {
            for x in 8..32 {
                img.put_pixel(x, y, Rgba([255, 0, 0, 255]));
            }
            for x in 32..56 {
                img.put_pixel(x, y, Rgba([0, 0, 255, 255]));
            }
        }
        let cfg = TraceConfig {
            mode: 1,
            colors: 4,
            ..TraceConfig::default()
        };
        let result = vectorize_rgba(img.as_raw(), 64, 64, &cfg).expect("vectorize");
        let (buf, w, h) = rasterize_svg(&result.svg).expect("raster");
        let mut redish = 0u32;
        let mut blueish = 0u32;
        for px in buf.chunks(4) {
            if px[3] < 8 {
                continue;
            }
            if px[0] > 180 && px[1] < 80 && px[2] < 80 {
                redish += 1;
            }
            if px[2] > 180 && px[0] < 80 && px[1] < 80 {
                blueish += 1;
            }
        }
        eprintln!(
            "color {w}x{h} redish={redish} blueish={blueish}\n{}",
            result.svg
        );
        assert!(redish > 100, "expected red region");
        assert!(blueish > 100, "expected blue region");
    }

    #[test]
    fn raster_is_straight_alpha() {
        // 白い円の縁（半透明）の RGB が 255 のまま残る（乗算済みなら alpha に比例して暗くなる）
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100">
            <circle cx="50" cy="50" r="30.5" fill="#ffffff"/></svg>"##;
        let (rgba, _, _) = rasterize_svg(svg).expect("raster");
        let edges: Vec<_> = rgba
            .chunks_exact(4)
            .filter(|p| p[3] > 20 && p[3] < 235)
            .collect();
        assert!(!edges.is_empty());
        for p in edges {
            assert!(p[0] >= 253 && p[1] >= 253 && p[2] >= 253, "縁が暗い: {p:?}");
        }
    }

    #[test]
    fn unpremultiply_clears_transparent_rgb() {
        let mut px = [10u8, 20, 30, 0, 64, 32, 0, 128];
        unpremultiply_rgba(&mut px);
        assert_eq!(px, [0, 0, 0, 0, 128, 64, 0, 128]);
    }

    #[test]
    fn color_input_is_straight() {
        // 半透明の画素は色をそのまま不透明にする（乗算済みとみなして割ると (199, 100, 40) に化ける）
        let mut img = RgbaImage::new(3, 1);
        img.put_pixel(0, 0, Rgba([100, 50, 20, 128]));
        img.put_pixel(1, 0, Rgba([10, 20, 30, 0]));
        img.put_pixel(2, 0, Rgba([7, 8, 9, 255]));
        let out = prepare_straight_keep_alpha(&img);
        assert_eq!(out.get_pixel(0, 0).0, [100, 50, 20, 255]);
        assert_eq!(out.get_pixel(1, 0).0, [0, 0, 0, 0]);
        assert_eq!(out.get_pixel(2, 0).0, [7, 8, 9, 255]);
    }

    #[test]
    fn binary_input_composites_straight_over_white() {
        // ストレートの (80, 80, 80, 200) を白に合成すると 118 でインク。乗算済みとみなすと 135 で白に落ちる
        let mut img = RgbaImage::new(4, 1);
        img.put_pixel(0, 0, Rgba([80, 80, 80, 200]));
        img.put_pixel(1, 0, Rgba([0, 0, 0, 0]));
        img.put_pixel(2, 0, Rgba([0, 0, 0, 255]));
        img.put_pixel(3, 0, Rgba([255, 255, 255, 128]));
        let out = to_binary_bw(&img, 128);
        let v: Vec<u8> = out.pixels().map(|p| p[0]).collect();
        assert_eq!(v, [0, 255, 0, 255]);
    }

    #[test]
    fn resize_keeps_straight_edge_color() {
        // 長辺が MAX_SIDE を超えると縮小される。透明部分の RGB（白）が縁ににじまないこと
        let w = MAX_SIDE * 2;
        let mut img = RgbaImage::from_pixel(w, 8, Rgba([255, 255, 255, 0]));
        for y in 0..8 {
            for x in 0..=(w / 2) {
                img.put_pixel(x, y, Rgba([200, 40, 40, 255]));
            }
        }
        let out = resize_if_needed(img);
        assert_eq!(out.width(), MAX_SIDE);
        let mut edges = 0;
        for p in out.pixels().filter(|p| p[3] > 0) {
            if p[3] < 255 {
                edges += 1;
            }
            assert!(
                p[0].abs_diff(200) <= 2 && p[1].abs_diff(40) <= 2 && p[2].abs_diff(40) <= 2,
                "縁ににじみ: {p:?}"
            );
        }
        assert!(edges > 0, "半透明の縁ができていない（検査が空振り）");
    }

    #[test]
    fn color_file_input_keeps_straight_color() {
        // PNG はストレート。半透明の面の色が明るく化けずにトレースされる
        let mut img = RgbaImage::from_pixel(64, 64, Rgba([0, 0, 0, 0]));
        for y in 8..56 {
            for x in 8..56 {
                img.put_pixel(x, y, Rgba([100, 50, 20, 128]));
            }
        }
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("straight_half_alpha.png");
        img.save(&path).expect("save png");
        let cfg = TraceConfig {
            mode: 1,
            colors: 4,
            ..TraceConfig::default()
        };
        let r = vectorize_file(&path, &cfg).expect("vectorize");
        let (buf, w, _) = rasterize_svg(&r.svg).expect("raster");
        let i = ((32 * w + 32) * 4) as usize;
        let p = &buf[i..i + 4];
        eprintln!("center {p:?}\n{}", r.svg);
        assert_eq!(p[3], 255);
        assert!(
            p[0].abs_diff(100) <= 16 && p[1].abs_diff(50) <= 16 && p[2].abs_diff(20) <= 16,
            "色が化けた: {p:?}"
        );
    }

    #[test]
    fn sanitize_stem_replaces_forbidden() {
        assert_eq!(sanitize_stem("a/b:c*.png"), "a_b_c_.png");
        assert_eq!(sanitize_stem("  ..  "), "image");
        assert_eq!(
            stem_from_image_path(std::path::Path::new(r"C:\foo\bar\logo.png")),
            "logo"
        );
    }

    #[test]
    fn resolve_filename_rules() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/export_name_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let layer = ExportMeta {
            stem: None,
            layer: Some(3),
            frame: Some(100),
        };
        assert_eq!(
            resolve_export_filename(&dir, &layer),
            "layer3_100.svg"
        );

        let file = ExportMeta {
            stem: Some("logo".into()),
            layer: None,
            frame: None,
        };
        assert_eq!(resolve_export_filename(&dir, &file), "logo_000.svg");
        std::fs::write(dir.join("logo_000.svg"), b"<svg/>").unwrap();
        assert_eq!(next_stem_sequence(&dir, "logo"), 1);
        assert_eq!(resolve_export_filename(&dir, &file), "logo_001.svg");

        assert_eq!(
            resolve_export_filename(&dir, &ExportMeta::default()),
            "latest.svg"
        );
    }

    #[test]
    fn export_dir_from_project_uses_svg_export() {
        let aup2 = std::path::Path::new(r"D:\works\clip\project.aup2");
        assert_eq!(
            export_dir_from_project(Some(aup2)),
            PathBuf::from(r"D:\works\clip\SVG_export")
        );
        // None は app_data 依存のためここでは呼ばない（ホスト未初期化）
    }
}
