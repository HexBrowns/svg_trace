//! ビットマップ → SVG トレース本体（mod2 / 共用ロジック）

use anyhow::{anyhow, bail, Context, Result};
use image::{imageops::FilterType, Rgba, RgbaImage};
use std::path::{Path, PathBuf};
use visioncortex::PathSimplifyMode;
use vtracer::{ColorImage, ColorMode, Config, Hierarchical};

pub const MAX_SIDE: u32 = 2048;
pub const MAX_COLORS: u32 = 16;
/// ファイル入力で読む画像の画素数の上限（1 億画素。RGBA で 400 MB）。これより大きい画像は読まずに断る。
/// どのみち長辺 `MAX_SIDE` に縮めてからトレースするので、先に縮めておけば済む
pub const MAX_DECODE_PIXELS: u64 = 100_000_000;

#[derive(Debug, Clone)]
pub struct TraceConfig {
    /// 0 = binary, 1 = color
    pub mode: i32,
    /// カラー時の目標色数 (2..=16)
    pub colors: i32,
    pub filter_speckle: i32,
    pub corner_threshold: i32,
    /// 二値の輝度閾値 (0..=255)
    pub binary_threshold: i32,
    /// ファイル入力時の出力 stem（拡張子なし）
    pub export_stem: Option<String>,
    /// レイヤー入力時のレイヤー番号
    pub export_layer: Option<i32>,
    /// レイヤー入力時のフレーム番号
    pub export_frame: Option<i32>,
    /// 「自動書き出し」。ON のときだけ名前付き SVG を書き出す（v0.1.3 から）
    pub auto_export: bool,
    /// キャッシュのスロット名（Lua の `obj.id`）。無ければ "" の 1 スロットを共有する
    pub instance: String,
    /// 入力のキー（入力元・パラメータ・大きさ）。スロットと一致したときだけキャッシュに当たる
    pub cache_key: String,
}

impl Default for TraceConfig {
    fn default() -> Self {
        Self {
            mode: 0,
            colors: 8,
            filter_speckle: 4,
            corner_threshold: 60,
            binary_threshold: 128,
            export_stem: None,
            export_layer: None,
            export_frame: None,
            auto_export: false,
            instance: String::new(),
            cache_key: String::new(),
        }
    }
}

impl TraceConfig {
    // aviutl2 0.47: from_param_table は Result。i32 は欠けたキーでも Ok(0)（0.29 の Some(0) と同じ）、
    // String は欠けていると Err（0.29 の None）なので .ok() で写す
    pub fn from_table(t: &aviutl2::module::ScriptModuleParamTable<'_>) -> Self {
        let int = |key: &str| i32::from_param_table(t, key).ok();
        let string = |key: &str| String::from_param_table(t, key).ok();
        let mut c = Self::default();
        if let Some(v) = int("mode") {
            c.mode = v;
        }
        if let Some(v) = int("colors") {
            c.colors = v.clamp(2, MAX_COLORS as i32);
        }
        if let Some(v) = int("filter_speckle") {
            c.filter_speckle = v.max(0);
        }
        if let Some(v) = int("corner_threshold") {
            c.corner_threshold = v.clamp(0, 180);
        }
        if let Some(v) = int("binary_threshold") {
            c.binary_threshold = v.clamp(0, 255);
        }
        // 欠けていれば 0 = OFF
        c.auto_export = int("auto_export").unwrap_or(0) != 0;
        if let Some(s) = string("instance") {
            c.instance = s;
        }
        if let Some(s) = string("cache_key") {
            c.cache_key = s;
        }
        // export_kind で明示（欠落キーの get_int=0 と区別する）
        match string("export_kind").as_deref().map(str::trim) {
            Some("file") => {
                if let Some(s) = string("export_stem") {
                    let s = s.trim();
                    if !s.is_empty() {
                        c.export_stem = Some(s.to_string());
                    }
                }
            }
            Some("layer") => {
                c.export_layer = int("export_layer");
                c.export_frame = int("export_frame");
            }
            _ => {}
        }
        c
    }

    pub fn export_meta(&self) -> ExportMeta {
        ExportMeta {
            stem: self.export_stem.clone(),
            layer: self.export_layer,
            frame: self.export_frame,
        }
    }
}

use aviutl2::module::FromScriptModuleParamTable;

#[derive(Debug, Clone)]
pub struct TraceResult {
    pub svg: String,
    pub width: u32,
    pub height: u32,
}

/// エクスポート命名用メタ（ファイル / レイヤー / フォールバック）
#[derive(Debug, Clone, Default)]
pub struct ExportMeta {
    pub stem: Option<String>,
    pub layer: Option<i32>,
    pub frame: Option<i32>,
}

/// 未保存時のフォールバック: Plugin/svg_trace/export
pub fn fallback_export_dir() -> PathBuf {
    aviutl2::config::app_data_path()
        .join("Plugin")
        .join("svg_trace")
        .join("export")
}

/// aux2 が書くサイドカー（1行 = 現在のエクスポートルート絶対パス）
pub fn export_root_sidecar_path() -> PathBuf {
    aviutl2::config::app_data_path()
        .join("Plugin")
        .join("svg_trace")
        .join("export_root.txt")
}

/// プロジェクト隣の SVG_export、またはフォールバック
#[allow(dead_code)] // ユニットテスト / 仕様上の公開ヘルパ
pub fn export_dir_from_project(project_path: Option<&Path>) -> PathBuf {
    if let Some(p) = project_path {
        if let Some(parent) = p.parent() {
            if !parent.as_os_str().is_empty() {
                return parent.join("SVG_export");
            }
        }
    }
    fallback_export_dir()
}

/// サイドカーにエクスポートルートを書き込む（aux2 側と対称の API）
#[allow(dead_code)]
pub fn write_export_root_sidecar(dir: &Path) -> Result<()> {
    let sidecar = export_root_sidecar_path();
    if let Some(parent) = sidecar.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&sidecar, dir.to_string_lossy().as_bytes())
        .with_context(|| format!("Failed to write {}", sidecar.display()))?;
    Ok(())
}

/// サイドカー優先。無ければ Plugin/svg_trace/export
pub fn export_dir() -> PathBuf {
    let sidecar = export_root_sidecar_path();
    if let Ok(raw) = std::fs::read_to_string(&sidecar) {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    fallback_export_dir()
}

pub fn latest_svg_path() -> PathBuf {
    export_dir().join("latest.svg")
}

/// Windows 禁則文字などを `_` に置換した安全な stem
pub fn sanitize_stem(raw: &str) -> String {
    let s: String = raw
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let s = s.trim().trim_matches('.').to_string();
    if s.is_empty() {
        "image".to_string()
    } else {
        s
    }
}

/// 画像パスから拡張子なし stem を得る
#[allow(dead_code)] // Lua 側でも算出するが Rust 側ヘルパとして公開
pub fn stem_from_image_path(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .map(sanitize_stem)
        .unwrap_or_else(|| "image".to_string())
}

/// ディレクトリ内の `{stem}_NNN.svg` の次の連番（000 始まり）。
///
/// 番号は 3 桁以上（999 の次は 1000）。v0.1.4 までは 999 で頭打ちになり、以後 `_999` を黙って上書きしていた
pub fn next_stem_sequence(dir: &Path, stem: &str) -> u32 {
    let prefix = format!("{stem}_");
    let mut max: Option<u32> = None;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(rest) = name.strip_prefix(&prefix) else {
            continue;
        };
        let Some(num_part) = rest.strip_suffix(".svg") else {
            continue;
        };
        if num_part.len() >= 3 && num_part.chars().all(|c| c.is_ascii_digit()) {
            if let Ok(n) = num_part.parse::<u32>() {
                max = Some(max.map_or(n, |m| m.max(n)));
            }
        }
    }
    match max {
        Some(n) => n.saturating_add(1),
        None => 0,
    }
}

/// 自動書き出しのファイル名の決め方
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportName {
    /// レイヤー入力: `layer{n}_{f}.svg`（同じ名前なら上書き）
    Fixed(String),
    /// ファイル入力: `{stem}_{連番}.svg`。引数は禁則文字を置き換えた stem
    Numbered(String),
    /// どちらでもない: 書き出し先の `latest.svg` だけ
    LatestOnly,
}

impl ExportMeta {
    pub fn export_name(&self) -> ExportName {
        if let (Some(layer), Some(frame)) = (self.layer, self.frame) {
            return ExportName::Fixed(format!("layer{layer}_{frame}.svg"));
        }
        match &self.stem {
            Some(stem) => ExportName::Numbered(sanitize_stem(stem)),
            None => ExportName::LatestOnly,
        }
    }
}

/// 連番のファイル名（3 桁以上）
pub fn numbered_file_name(stem: &str, n: u32) -> String {
    format!("{stem}_{n:03}.svg")
}

// 入力はファイル（`image` の RGBA8）もレイヤー（`obj.getpixeldata`）もストレートアルファ。
// 乗算済みとみなして扱うと、半透明の画素（アンチエイリアスの縁など）の色が化ける。

/// 長辺が MAX_SIDE を超えたら縮小する。
///
/// `imageops::resize` は乗算済みを前提にしている（同関数の doc）。ストレートのまま補間すると
/// 透明部分の RGB が縁ににじむので、乗算してから縮小して戻す。
pub(crate) fn resize_if_needed(mut img: RgbaImage) -> RgbaImage {
    let (w, h) = img.dimensions();
    let Some((nw, nh)) = scaled_size(w, h) else {
        return img;
    };
    premultiply_rgba(&mut img);
    let mut out = image::imageops::resize(&img, nw, nh, FilterType::Triangle);
    unpremultiply_rgba(&mut out);
    out
}

/// 長辺が MAX_SIDE を超えるときの縮めた後の大きさ。超えなければ None
fn scaled_size(w: u32, h: u32) -> Option<(u32, u32)> {
    let long = w.max(h);
    if long <= MAX_SIDE {
        return None;
    }
    let scale = MAX_SIDE as f32 / long as f32;
    let nw = ((w as f32) * scale).round().max(1.0) as u32;
    let nh = ((h as f32) * scale).round().max(1.0) as u32;
    Some((nw, nh))
}

/// 半透明の画素を色はそのままで不透明にし、完全透明だけ α=0 で残す（vtracer の透明キーは α=0 だけ）。
pub(crate) fn prepare_straight_keep_alpha(img: &RgbaImage) -> RgbaImage {
    let (w, h) = img.dimensions();
    let mut out = RgbaImage::new(w, h);
    for (x, y, p) in img.enumerate_pixels() {
        if p[3] == 0 {
            out.put_pixel(x, y, Rgba([0, 0, 0, 0]));
        } else {
            out.put_pixel(x, y, Rgba([p[0], p[1], p[2], 255]));
        }
    }
    out
}

/// 二値用: 透明を白背景に合成してから輝度二値化（黒/白の不透明）。
/// ※ Binary + 透明キーは vtracer が面を潰すことがあるため白背景方式。
pub(crate) fn to_binary_bw(img: &RgbaImage, threshold: u8) -> RgbaImage {
    let (w, h) = img.dimensions();
    let mut out = RgbaImage::new(w, h);
    for (x, y, p) in img.enumerate_pixels() {
        let (r, g, b) = if p[3] == 0 {
            (255u8, 255u8, 255u8)
        } else if p[3] < 255 {
            let a = p[3] as f32 / 255.0;
            // ストレート → 白へ合成（c * a + 白 * (1 - a)）
            (
                (p[0] as f32 * a + 255.0 * (1.0 - a)).round().clamp(0.0, 255.0) as u8,
                (p[1] as f32 * a + 255.0 * (1.0 - a)).round().clamp(0.0, 255.0) as u8,
                (p[2] as f32 * a + 255.0 * (1.0 - a)).round().clamp(0.0, 255.0) as u8,
            )
        } else {
            (p[0], p[1], p[2])
        };
        let yv = (0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32) as u8;
        let v = if yv < threshold { 0 } else { 255 };
        out.put_pixel(x, y, Rgba([v, v, v, 255]));
    }
    out
}

fn rgba_to_color_image(img: &RgbaImage) -> ColorImage {
    let (w, h) = img.dimensions();
    ColorImage {
        pixels: img.as_raw().clone(),
        width: w as usize,
        height: h as usize,
    }
}

fn make_vtracer_config(cfg: &TraceConfig) -> Config {
    let binary = cfg.mode == 0;
    let colors = cfg.colors.clamp(2, MAX_COLORS as i32) as u32;
    let color_precision = if binary {
        6
    } else {
        match colors {
            2..=3 => 3,
            4..=6 => 4,
            7..=10 => 5,
            _ => 6,
        }
    };
    let layer_difference = if binary {
        16
    } else {
        (256 / colors).clamp(8, 48) as i32
    };
    Config {
        color_mode: if binary {
            ColorMode::Binary
        } else {
            ColorMode::Color
        },
        // Cutout は入れ子を穴にする → ソリッド黒や色面が透明化しやすい
        hierarchical: Hierarchical::Stacked,
        filter_speckle: cfg.filter_speckle.max(0) as usize,
        color_precision,
        layer_difference,
        mode: PathSimplifyMode::Spline,
        corner_threshold: cfg.corner_threshold,
        length_threshold: 4.0,
        max_iterations: 10,
        splice_threshold: 45,
        path_precision: Some(2),
    }
}

/// 二値SVGから白（またはほぼ白）の背景パスを除き、インクだけ残す。
fn strip_near_white_fills(svg: &str) -> String {
    let mut out = String::with_capacity(svg.len());
    let mut rest = svg;
    while let Some(start) = rest.find("<path") {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        let end_rel = after.find("/>").or_else(|| after.find("</path>"));
        let Some(end_rel) = end_rel else {
            out.push_str(after);
            return out;
        };
        let end = if after[end_rel..].starts_with("/>") {
            end_rel + 2
        } else {
            end_rel + "</path>".len()
        };
        let path = &after[..end];
        let is_white = path.contains("fill=\"#FFFFFF\"")
            || path.contains("fill=\"#ffffff\"")
            || path.contains("fill=\"white\"");
        if !is_white {
            out.push_str(path);
        }
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

/// 縮めた後の画像を、モードに合わせて vtracer へ渡す形にする
fn prepare_image(img: &RgbaImage, cfg: &TraceConfig) -> RgbaImage {
    if cfg.mode == 0 {
        to_binary_bw(img, cfg.binary_threshold as u8)
    } else {
        prepare_straight_keep_alpha(img)
    }
}

fn convert_prepared(img: RgbaImage, cfg: &TraceConfig) -> Result<TraceResult> {
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        bail!("empty image");
    }
    let color_img = rgba_to_color_image(&img);
    let vt_cfg = make_vtracer_config(cfg);
    // vtracer（visioncortex）は、二値で「かたまり」が 65535 個を超えると panic する
    // （`BinaryImage::to_clusters` の `panic!("overflow")`。写真を FHD のまま二値にすると起きる）。
    // panic のままだと結果が残らず、毎フレームトレースし直して失敗を繰り返すので、失敗として返す
    let converted = std::panic::catch_unwind(move || vtracer::convert(color_img, vt_cfg));
    let svg_file = match converted {
        Ok(r) => r.map_err(|e| anyhow!("vtracer: {e}"))?,
        Err(payload) => {
            let what = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_default();
            if what == "overflow" {
                let hint = if cfg.mode == 0 {
                    "スペックル除去を上げるか、二値閾値を変えるか、カラーにしてください"
                } else {
                    "スペックル除去を上げるか、色数を減らしてください"
                };
                bail!("絵が細かすぎてトレースできない（{w}x{h}。かたまりが 65535 個を超えた）。{hint}");
            }
            bail!("トレースに失敗した（vtracer: {what}）");
        }
    };
    let mut svg = svg_file.to_string();
    if cfg.mode == 0 {
        svg = strip_near_white_fills(&svg);
    }
    Ok(TraceResult {
        svg,
        width: w,
        height: h,
    })
}

/// 縮めた後の画像（`load_image_file` / `resize_if_needed` を通したもの）をトレースする
pub fn vectorize_image(img: &RgbaImage, cfg: &TraceConfig) -> Result<TraceResult> {
    convert_prepared(prepare_image(img, cfg), cfg)
}

/// レイヤー入力の画素の中身を表すキー（大きさと xxh3 の 128 bit）。
///
/// SVG_TRACE.obj2 v0.1.3 まではキャッシュのキーにフレーム番号を入れていたので、下のレイヤーが静止画でも
/// 毎フレームトレースし直し、同じフレームで下のレイヤーを編集しても描き直さなかった。中身で引けば両方が直る
pub fn content_key(pixels: &[u8], width: u32, height: u32) -> String {
    let len = (width as usize)
        .saturating_mul(height as usize)
        .saturating_mul(4)
        .min(pixels.len());
    let h = xxhash_rust::xxh3::xxh3_128(&pixels[..len]);
    format!("{width}x{height}:{h:032x}")
}

pub fn vectorize_rgba(pixels: &[u8], width: u32, height: u32, cfg: &TraceConfig) -> Result<TraceResult> {
    let expect = (width as usize)
        .saturating_mul(height as usize)
        .saturating_mul(4);
    if pixels.len() < expect || width == 0 || height == 0 {
        bail!(
            "invalid rgba buffer: len={} expect={} {}x{}",
            pixels.len(),
            expect,
            width,
            height
        );
    }
    let img = RgbaImage::from_raw(width, height, pixels[..expect].to_vec())
        .ok_or_else(|| anyhow!("from_raw failed"))?;
    vectorize_image(&resize_if_needed(img), cfg)
}

#[allow(dead_code)] // テストと測定から使う。mod2 は読んだ画像をキャッシュする（`lib.rs`）
pub fn vectorize_file(path: &Path, cfg: &TraceConfig) -> Result<TraceResult> {
    vectorize_image(&load_image_file(path)?, cfg)
}

/// 画像ファイルを読み、長辺 `MAX_SIDE` に縮めた RGBA（ストレート）にする。
///
/// 先に見出しだけを読んで大きさを確かめ、`MAX_DECODE_PIXELS` を超える画像は展開せずに断る
/// （v0.1.4 までは 16384² でも全部を展開してから縮めていた）。アルファの無い画像は元の形式のまま縮めてから
/// RGBA にするので、写真を読むときに元の大きさの RGBA の複製を作らない
pub fn load_image_file(path: &Path) -> Result<RgbaImage> {
    let open = || {
        image::ImageReader::open(path)
            .with_context(|| format!("画像を開けない: {}", path.display()))?
            .with_guessed_format()
            .with_context(|| format!("画像を開けない: {}", path.display()))
    };
    let (w, h) = open()?
        .into_dimensions()
        .with_context(|| format!("画像の大きさを読めない: {}", path.display()))?;
    if u64::from(w) * u64::from(h) > MAX_DECODE_PIXELS {
        bail!(
            "画像が大きすぎる（{w}x{h}）。{} 万画素までにしてください（長辺 {MAX_SIDE} に縮めてからトレースするので、先に縮めておくと速い）: {}",
            MAX_DECODE_PIXELS / 10_000,
            path.display()
        );
    }
    let img = open()?
        .decode()
        .with_context(|| format!("画像を読めない: {}", path.display()))?;
    if img.color().has_alpha() {
        return Ok(resize_if_needed(img.into_rgba8()));
    }
    // アルファが無ければ乗算済みかどうかの区別が要らないので、そのまま縮める
    let img = match scaled_size(img.width(), img.height()) {
        Some((nw, nh)) => img.resize_exact(nw, nh, FilterType::Triangle),
        None => img,
    };
    Ok(img.into_rgba8())
}

pub fn rasterize_svg(svg: &str) -> Result<(Vec<u8>, u32, u32)> {
    let opt = resvg::usvg::Options::default();
    let tree = resvg::usvg::Tree::from_str(svg, &opt).map_err(|e| anyhow!("usvg: {e}"))?;
    let size = tree.size();
    let w = size.width().ceil().max(1.0) as u32;
    let h = size.height().ceil().max(1.0) as u32;
    let mut pixmap =
        resvg::tiny_skia::Pixmap::new(w, h).ok_or_else(|| anyhow!("pixmap {w}x{h}"))?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::identity(),
        &mut pixmap.as_mut(),
    );
    // tiny-skia の Pixmap は乗算済み。obj.putpixeldata の RGBA32bit はストレートなので戻してから渡す
    let mut rgba = pixmap.take();
    unpremultiply_rgba(&mut rgba);
    Ok((rgba, w, h))
}

/// ストレートの RGBA を乗算済みにする（`unpremultiply_rgba` の逆）。
fn premultiply_rgba(pixels: &mut [u8]) {
    for pixel in pixels.chunks_exact_mut(4) {
        let alpha = u16::from(pixel[3]);
        for channel in &mut pixel[..3] {
            *channel = ((u16::from(*channel) * alpha + 127) / 255) as u8;
        }
    }
}

/// 乗算済み RGBA をストレートに戻す（tiny-skia の出力と、入力の縮小後に使う）。
///
/// ホストの `PIXEL_RGBA` と Lua の `obj.putpixeldata` はストレートアルファ。
/// 乗算済みのまま渡すと縁と半透明部分が暗くなる（本家 svg.aux2 v0.5.1、`svg_txt` と同じ式）
pub fn unpremultiply_rgba(pixels: &mut [u8]) {
    for pixel in pixels.chunks_exact_mut(4) {
        let alpha = u16::from(pixel[3]);
        if alpha == 0 {
            pixel[..3].fill(0);
            continue;
        }
        for channel in &mut pixel[..3] {
            let straight = (u16::from(*channel) * 255 + alpha / 2) / alpha;
            *channel = straight.min(255) as u8;
        }
    }
}
