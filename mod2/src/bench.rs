//! 机上の測定（実用の規模で、1 回のトレース・描き直し・キャッシュに当たる経路に掛かる時間）。
//! 時間は機械で変わるので、既定では走らせない:
//!   cargo test --release -p svg-trace-mod2 bench -- --ignored --nocapture --test-threads=1
//! 対象を絞るときは環境変数 `SVG_TRACE_BENCH`（例: `logo`, `fhd`, `noise`）に、名前に含まれる語を書く。

use crate::store::{SharedStore, SourceStamp};
use crate::trace::{content_key, rasterize_svg, vectorize_image, vectorize_rgba, TraceConfig};
use std::path::PathBuf;
use std::time::Instant;

fn wanted(name: &str) -> bool {
    match std::env::var("SVG_TRACE_BENCH") {
        Ok(f) if !f.is_empty() => f.split(',').any(|w| name.contains(w.trim())),
        _ => true,
    }
}

/// 白地に赤い輪と黒い帯（単色ロゴ）
fn logo(w: u32, h: u32) -> Vec<u8> {
    let mut v = vec![255u8; (w * h * 4) as usize];
    let (cx, cy) = (w as f32 * 0.5, h as f32 * 0.5);
    let r = w.min(h) as f32 * 0.35;
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            let d = ((x as f32 - cx).powi(2) + (y as f32 - cy).powi(2)).sqrt();
            let ring = d < r && d > r * 0.7;
            let bar = (y as f32 - cy).abs() < r * 0.12 && (x as f32 - cx).abs() < r * 0.9;
            if ring {
                v[i..i + 3].copy_from_slice(&[220, 30, 30]);
            } else if bar {
                v[i..i + 3].copy_from_slice(&[20, 20, 20]);
            }
        }
    }
    v
}

/// 横と縦のなめらかなグラデーション
fn gradient(w: u32, h: u32) -> Vec<u8> {
    let mut v = vec![255u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            v[i] = (x * 255 / w.max(1)) as u8;
            v[i + 1] = (y * 255 / h.max(1)) as u8;
            v[i + 2] = ((x + y) * 255 / (w + h).max(1)) as u8;
        }
    }
    v
}

/// 画素ごとの乱数（写真の細かい部分の最悪の場合）
fn noise(w: u32, h: u32) -> Vec<u8> {
    let mut v = vec![255u8; (w * h * 4) as usize];
    let mut s: u32 = 0x9E37_79B9;
    for px in v.chunks_exact_mut(4) {
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        px[0] = s as u8;
        px[1] = (s >> 8) as u8;
        px[2] = (s >> 16) as u8;
    }
    v
}

fn cfg(mode: i32, colors: i32) -> TraceConfig {
    TraceConfig {
        mode,
        colors,
        ..TraceConfig::default()
    }
}

const SIZES: [(&str, u32, u32); 4] = [
    ("512", 512, 512),
    ("fhd", 1920, 1080),
    ("2048", 2048, 2048),
    ("4k", 3840, 2160),
];

#[allow(clippy::type_complexity)]
const CONTENTS: [(&str, fn(u32, u32) -> Vec<u8>); 3] = [("logo", logo), ("gradient", gradient), ("noise", noise)];

const MODES: [(&str, i32, i32); 4] = [("binary", 0, 8), ("color2", 1, 2), ("color8", 1, 8), ("color16", 1, 16)];

/// トレース（`vectorize_rgba`）と、その結果を描く（`rasterize_svg`）時間
#[test]
#[ignore]
fn bench_trace_and_rasterize() {
    println!("| 大きさ | 絵 | モード | トレース ms | SVG KB | 描く ms |");
    println!("|---|---|---|---|---|---|");
    for (sname, w, h) in SIZES {
        for (cname, make) in CONTENTS {
            let pixels = make(w, h);
            for (mname, mode, colors) in MODES {
                let name = format!("{sname}/{cname}/{mname}");
                if !wanted(&name) {
                    continue;
                }
                let t0 = Instant::now();
                let c = cfg(mode, colors);
                let r = std::panic::catch_unwind(|| vectorize_rgba(&pixels, w, h, &c));
                let trace_ms = t0.elapsed().as_secs_f64() * 1000.0;
                let r = match r {
                    Ok(Ok(r)) => r,
                    Ok(Err(e)) => {
                        println!("| {sname} | {cname} | {mname} | {trace_ms:.0}（失敗: {e:#}） | - | - |");
                        continue;
                    }
                    Err(_) => {
                        println!("| {sname} | {cname} | {mname} | {trace_ms:.0}（panic） | - | - |");
                        continue;
                    }
                };
                let t1 = Instant::now();
                let _ = rasterize_svg(&r.svg).expect("raster");
                let raster_ms = t1.elapsed().as_secs_f64() * 1000.0;
                println!(
                    "| {sname} | {cname} | {mname} | {trace_ms:.0} | {:.0} | {raster_ms:.0} |",
                    r.svg.len() as f64 / 1024.0
                );
            }
        }
    }
}

/// キャッシュに当たる経路（`render_cached`）。1 回目は描き、2 回目からは描いた画素を返す。
/// ファイル入力（元画像の更新日時を確かめる）も測る
#[test]
#[ignore]
fn bench_render_cached_hit() {
    let (w, h) = (1920, 1080);
    let c = TraceConfig {
        instance: "1".into(),
        cache_key: "k".into(),
        mode: 1,
        colors: 8,
        ..TraceConfig::default()
    };
    let r = vectorize_rgba(&logo(w, h), w, h, &c).unwrap();
    for (label, source) in [("レイヤー入力", None), ("ファイル入力", Some(()))] {
        let store = SharedStore::new();
        let stamp = source.map(|()| {
            let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("bench");
            std::fs::create_dir_all(&dir).unwrap();
            let p = dir.join("stamp.png");
            std::fs::write(&p, b"x").unwrap();
            SourceStamp::of(&p).unwrap()
        });
        store.store_result(Ok(r.clone()), &c, stamp, None, Instant::now());
        let t0 = Instant::now();
        store.render_cached("1", "k", Instant::now()).hit().unwrap();
        let first = t0.elapsed().as_secs_f64() * 1000.0;
        let n = 1000;
        let t1 = Instant::now();
        for _ in 0..n {
            let (raster, _, _) = store.render_cached("1", "k", Instant::now()).hit().unwrap();
            store.lend(1, raster, Instant::now());
        }
        let per = t1.elapsed().as_secs_f64() * 1000.0 / n as f64;
        println!(
            "[INFO] render_cached FHD ロゴ カラー 8 色（{label}）: 1 回目（描く） {first:.1} ms / 2 回目から {:.1} us",
            per * 1000.0
        );
    }
}

/// レイヤー入力の画素の中身のキー（毎フレーム計算する）
#[test]
#[ignore]
fn bench_content_key() {
    for (sname, w, h) in SIZES {
        let pixels = noise(w, h);
        let n = 50;
        let t0 = Instant::now();
        for _ in 0..n {
            std::hint::black_box(content_key(&pixels, w, h));
        }
        let per = t0.elapsed().as_secs_f64() * 1000.0 / n as f64;
        println!("[INFO] content_key {sname}: {per:.2} ms");
    }
}

/// ファイル入力でスライダーを 5 段動かす（4K の PNG。段ごとにトレースし直す）。
/// v0.2.0 は読んだ画像を持っておくので、2 段目から読み直さない
#[test]
#[ignore]
fn bench_file_slider_steps() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("bench");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("logo_4k.png");
    let (w, h) = (3840, 2160);
    image::RgbaImage::from_raw(w, h, logo(w, h)).unwrap().save(&path).unwrap();
    let store = SharedStore::new();
    let mut total = 0.0;
    for step in 0..5 {
        let c = TraceConfig {
            mode: 1,
            colors: 8,
            corner_threshold: 60 + step,
            ..TraceConfig::default()
        };
        let t0 = Instant::now();
        let (img, _) = store.decoded_image(&path, Instant::now()).unwrap();
        vectorize_image(&img, &c).unwrap();
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        total += ms;
        println!("[INFO] ファイル入力 4K PNG カラー 8 色 {step} 段目: {ms:.0} ms");
    }
    println!("[INFO] ファイル入力 4K PNG 5 段の平均: {:.0} ms", total / 5.0);
}

/// レイヤー入力の模擬: 下のレイヤーが静止画のまま 300 フレーム再生する。
/// obj2 v0.1.3 のキー（フレーム番号入り）では毎フレームトレースし直していた。v0.2.0 は画素の中身で引く
#[test]
#[ignore]
fn bench_layer_static_300_frames() {
    for (sname, w, h) in [("512", 512u32, 512u32), ("fhd", 1920, 1080)] {
        let pixels = logo(w, h);
        let store = SharedStore::new();
        let mut traces = 0;
        let mut worst = 0.0f64;
        let t0 = Instant::now();
        for _frame in 0..300 {
            let tf = Instant::now();
            // SVG_TRACE.obj2 v0.2.0: "layer:{n}:{content_key}|..."
            let key = format!("layer:2:{}|1|8", content_key(&pixels, w, h));
            if store.render_cached("1", &key, Instant::now()).hit().is_none() {
                let c = TraceConfig {
                    instance: "1".into(),
                    cache_key: key.clone(),
                    mode: 1,
                    colors: 8,
                    ..TraceConfig::default()
                };
                let r = vectorize_rgba(&pixels, w, h, &c).unwrap();
                store.store_result(Ok(r), &c, None, None, Instant::now());
                store.render_cached("1", &key, Instant::now()).hit().unwrap();
                traces += 1;
            }
            worst = worst.max(tf.elapsed().as_secs_f64() * 1000.0);
        }
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        println!("[INFO] レイヤー入力 {sname} 静止画 300 フレーム: トレース {traces} 回 / 計 {ms:.0} ms / 最も長いフレーム {worst:.0} ms");
        assert_eq!(traces, 1);
    }
}
