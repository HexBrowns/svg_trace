#[cfg(test)]
mod tests {
    use crate::trace::{
        content_key, export_dir_from_project, load_image_file, next_stem_sequence,
        numbered_file_name, prepare_straight_keep_alpha, rasterize_svg, resize_if_needed,
        sanitize_stem, stem_from_image_path, to_binary_bw, unpremultiply_rgba, vectorize_file,
        vectorize_rgba, ExportMeta, ExportName, TraceConfig, MAX_SIDE,
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
    fn export_name_rules() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/export_name_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let layer = ExportMeta {
            stem: None,
            layer: Some(3),
            frame: Some(100),
        };
        assert_eq!(layer.export_name(), ExportName::Fixed("layer3_100.svg".into()));

        let file = ExportMeta {
            stem: Some("lo:go".into()),
            layer: None,
            frame: None,
        };
        assert_eq!(file.export_name(), ExportName::Numbered("lo_go".into()));
        assert_eq!(ExportMeta::default().export_name(), ExportName::LatestOnly);

        assert_eq!(next_stem_sequence(&dir, "logo"), 0);
        std::fs::write(dir.join("logo_000.svg"), b"<svg/>").unwrap();
        assert_eq!(next_stem_sequence(&dir, "logo"), 1);
        // 999 の次は 1000（v0.1.4 までは 999 で止まり、上書きしていた）
        std::fs::write(dir.join("logo_999.svg"), b"<svg/>").unwrap();
        assert_eq!(next_stem_sequence(&dir, "logo"), 1000);
        assert_eq!(numbered_file_name("logo", 1000), "logo_1000.svg");
        std::fs::write(dir.join("logo_1000.svg"), b"<svg/>").unwrap();
        assert_eq!(next_stem_sequence(&dir, "logo"), 1001);
        // 別の画像名や 2 桁は数えない
        std::fs::write(dir.join("logo2_005.svg"), b"<svg/>").unwrap();
        std::fs::write(dir.join("logo_99999x.svg"), b"<svg/>").unwrap();
        assert_eq!(next_stem_sequence(&dir, "logo"), 1001);
        assert_eq!(numbered_file_name("logo", 7), "logo_007.svg");
    }

    /// 写真を FHD のまま二値にすると vtracer が panic する（かたまりが 65535 個を超える）。
    /// panic ではなく、理由の分かる失敗として返す
    #[test]
    fn binary_overflow_is_an_error_not_a_panic() {
        let (w, h) = (1920u32, 1080u32);
        let mut img = RgbaImage::new(w, h);
        // 市松模様（1 画素ごと）で、かたまりが画素数の半分になる
        for (x, y, p) in img.enumerate_pixels_mut() {
            let v = if (x + y) % 2 == 0 { 0 } else { 255 };
            *p = Rgba([v, v, v, 255]);
        }
        let cfg = TraceConfig {
            mode: 0,
            filter_speckle: 0,
            ..TraceConfig::default()
        };
        let err = vectorize_rgba(img.as_raw(), w, h, &cfg).expect_err("失敗するはず");
        let msg = format!("{err:#}");
        assert!(msg.contains("65535"), "理由が分からない: {msg}");
    }

    #[test]
    fn content_key_follows_pixels() {
        let a = vec![1u8; 16];
        let mut b = a.clone();
        assert_eq!(content_key(&a, 2, 2), content_key(&b, 2, 2));
        b[5] = 2;
        assert_ne!(content_key(&a, 2, 2), content_key(&b, 2, 2));
        // 同じ画素列でも大きさが違えば別
        assert_ne!(content_key(&a, 2, 2), content_key(&a, 4, 1));
    }

    /// 大きすぎる画像は展開せずに断る（見出しだけ読む）
    #[test]
    fn huge_image_is_refused_before_decoding() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("huge_header.png");
        // 20000x20000 の PNG の見出し（IHDR）だけを書く。展開しようとすれば中身が無くて失敗する
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(b"IHDR");
        ihdr.extend_from_slice(&20000u32.to_be_bytes());
        ihdr.extend_from_slice(&20000u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(&ihdr);
        png.extend_from_slice(&crc32(&ihdr).to_be_bytes());
        // 見出しの読み取りは最初の IDAT まで進むので、空の IDAT と IEND を足す
        for name in [b"IDAT", b"IEND"] {
            png.extend_from_slice(&0u32.to_be_bytes());
            png.extend_from_slice(name);
            png.extend_from_slice(&crc32(name).to_be_bytes());
        }
        std::fs::write(&path, &png).unwrap();
        let err = load_image_file(&path).expect_err("断るはず");
        let msg = format!("{err:#}");
        assert!(msg.contains("大きすぎる") && msg.contains("20000x20000"), "{msg}");
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &b in data {
            crc ^= b as u32;
            for _ in 0..8 {
                crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
            }
        }
        !crc
    }

    /// アルファの無い大きな画像は元の形式のまま縮めて読む（縮めた後の大きさと色）
    #[test]
    fn opaque_large_image_is_resized_on_load() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("opaque_large.png");
        image::RgbImage::from_pixel(MAX_SIDE * 2, 100, image::Rgb([10, 200, 30]))
            .save(&path)
            .unwrap();
        let img = load_image_file(&path).unwrap();
        assert_eq!(img.dimensions(), (MAX_SIDE, 50));
        assert_eq!(img.get_pixel(10, 10).0, [10, 200, 30, 255]);
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
